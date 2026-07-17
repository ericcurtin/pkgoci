// Package main exposes containerd's registry distribution stack
// (core/remotes/docker) to Rust via a C archive. All registry protocol
// handling — token auth, manifest resolution, blob transfer — is
// containerd's code, not a reimplementation.
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"
	"unsafe"

	"github.com/containerd/containerd/v2/core/remotes"
	"github.com/containerd/containerd/v2/core/remotes/docker"
	"github.com/containerd/errdefs"
	"github.com/containerd/platforms"
	"github.com/opencontainers/go-digest"
	ocispec "github.com/opencontainers/image-spec/specs-go/v1"
	"github.com/sirupsen/logrus"
)

func init() {
	// containerd logs transport details via logrus; keep the CLI quiet.
	logrus.SetOutput(io.Discard)
}

const mediaTypeDockerManifestList = "application/vnd.docker.distribution.manifest.list.v2+json"

func newResolver() remotes.Resolver {
	authorizer := docker.NewDockerAuthorizer(docker.WithAuthCreds(
		func(host string) (string, string, error) {
			return os.Getenv("PKGOCI_USERNAME"), os.Getenv("PKGOCI_PASSWORD"), nil
		}))
	hosts := docker.ConfigureDefaultRegistries(
		docker.WithPlainHTTP(docker.MatchLocalhost),
		docker.WithAuthorizer(authorizer),
	)
	return docker.NewResolver(docker.ResolverOptions{Hosts: hosts})
}

func jsonResult(v any, err error) *C.char {
	if err != nil {
		b, _ := json.Marshal(map[string]string{"error": err.Error()})
		return C.CString(string(b))
	}
	if v == nil {
		v = map[string]bool{"ok": true}
	}
	b, err := json.Marshal(v)
	if err != nil {
		b, _ = json.Marshal(map[string]string{"error": err.Error()})
	}
	return C.CString(string(b))
}

func fetchAll(ctx context.Context, fetcher remotes.Fetcher, desc ocispec.Descriptor) ([]byte, error) {
	rc, err := fetcher.Fetch(ctx, desc)
	if err != nil {
		return nil, err
	}
	defer rc.Close()
	verifier := desc.Digest.Verifier()
	data, err := io.ReadAll(io.TeeReader(rc, verifier))
	if err != nil {
		return nil, err
	}
	if !verifier.Verified() {
		return nil, fmt.Errorf("digest mismatch fetching %s", desc.Digest)
	}
	return data, nil
}

type resolveResult struct {
	Digest   string          `json:"digest"`
	Manifest json.RawMessage `json:"manifest"`
	Index    json.RawMessage `json:"index,omitempty"`
}

// PkgociResolve resolves a reference and, if it is an index, descends into
// the manifest matching os/arch using containerd's platform matcher.
//
//export PkgociResolve
func PkgociResolve(cRef, cOS, cArch *C.char) *C.char {
	ref, osName, arch := C.GoString(cRef), C.GoString(cOS), C.GoString(cArch)
	ctx := context.Background()
	resolver := newResolver()

	name, desc, err := resolver.Resolve(ctx, ref)
	if err != nil {
		return jsonResult(nil, err)
	}
	fetcher, err := resolver.Fetcher(ctx, name)
	if err != nil {
		return jsonResult(nil, err)
	}
	body, err := fetchAll(ctx, fetcher, desc)
	if err != nil {
		return jsonResult(nil, err)
	}

	res := resolveResult{Digest: desc.Digest.String(), Manifest: body}
	if desc.MediaType == ocispec.MediaTypeImageIndex || desc.MediaType == mediaTypeDockerManifestList {
		var index ocispec.Index
		if err := json.Unmarshal(body, &index); err != nil {
			return jsonResult(nil, err)
		}
		matcher := platforms.NewMatcher(ocispec.Platform{OS: osName, Architecture: arch})
		var chosen *ocispec.Descriptor
		var available []string
		for i, m := range index.Manifests {
			if m.Platform == nil {
				continue
			}
			if m.Platform.OS != "unknown" {
				available = append(available, platforms.Format(*m.Platform))
			}
			if chosen == nil && matcher.Match(*m.Platform) {
				chosen = &index.Manifests[i]
			}
		}
		if chosen == nil {
			return jsonResult(nil, fmt.Errorf("%s has no build for %s/%s (available: %s)",
				ref, osName, arch, strings.Join(available, ", ")))
		}
		mbody, err := fetchAll(ctx, fetcher, *chosen)
		if err != nil {
			return jsonResult(nil, err)
		}
		res = resolveResult{Digest: chosen.Digest.String(), Manifest: mbody, Index: body}
	}
	return jsonResult(res, nil)
}

// PkgociFetchBlob streams a digest-verified blob to dest.
//
//export PkgociFetchBlob
func PkgociFetchBlob(cRef, cDigest, cMediaType *C.char, size C.longlong, cDest *C.char) *C.char {
	ref, dest := C.GoString(cRef), C.GoString(cDest)
	desc := ocispec.Descriptor{
		MediaType: C.GoString(cMediaType),
		Digest:    digest.Digest(C.GoString(cDigest)),
		Size:      int64(size),
	}
	ctx := context.Background()
	resolver := newResolver()
	fetcher, err := resolver.Fetcher(ctx, ref)
	if err != nil {
		return jsonResult(nil, err)
	}
	rc, err := fetcher.Fetch(ctx, desc)
	if err != nil {
		return jsonResult(nil, err)
	}
	defer rc.Close()

	tmp := dest + ".part"
	f, err := os.Create(tmp)
	if err != nil {
		return jsonResult(nil, err)
	}
	verifier := desc.Digest.Verifier()
	_, err = io.Copy(f, io.TeeReader(rc, verifier))
	f.Close()
	if err == nil && !verifier.Verified() {
		err = fmt.Errorf("digest mismatch for %s: expected %s", dest, desc.Digest)
	}
	if err != nil {
		os.Remove(tmp)
		return jsonResult(nil, err)
	}
	return jsonResult(nil, os.Rename(tmp, dest))
}

func pushWriter(ctx context.Context, ref string, desc ocispec.Descriptor, r io.Reader) error {
	pusher, err := newResolver().Pusher(ctx, ref)
	if err != nil {
		return err
	}
	w, err := pusher.Push(ctx, desc)
	if errdefs.IsAlreadyExists(err) {
		return nil
	}
	if err != nil {
		return err
	}
	defer w.Close()
	if _, err := io.Copy(w, r); err != nil {
		return err
	}
	return w.Commit(ctx, desc.Size, desc.Digest)
}

// PkgociPushBlob uploads a file as a blob (no-op if the digest exists).
//
//export PkgociPushBlob
func PkgociPushBlob(cRef, cDigest *C.char, size C.longlong, cPath *C.char) *C.char {
	f, err := os.Open(C.GoString(cPath))
	if err != nil {
		return jsonResult(nil, err)
	}
	defer f.Close()
	desc := ocispec.Descriptor{
		MediaType: "application/octet-stream",
		Digest:    digest.Digest(C.GoString(cDigest)),
		Size:      int64(size),
	}
	return jsonResult(nil, pushWriter(context.Background(), C.GoString(cRef), desc, f))
}

// PkgociPushManifest uploads a manifest/index under the tag in ref.
//
//export PkgociPushManifest
func PkgociPushManifest(cRef, cMediaType, cBody *C.char) *C.char {
	body := []byte(C.GoString(cBody))
	desc := ocispec.Descriptor{
		MediaType: C.GoString(cMediaType),
		Digest:    digest.FromBytes(body),
		Size:      int64(len(body)),
	}
	return jsonResult(nil, pushWriter(context.Background(), C.GoString(cRef), desc, strings.NewReader(string(body))))
}

//export PkgociFree
func PkgociFree(p *C.char) {
	C.free(unsafe.Pointer(p))
}

func main() {}
