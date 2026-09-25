// The Linux checks of ci.yml, as Dagger checks.
//
// Each check mirrors one ci.yml step and must stay equivalent to it: same
// command, same flags, same toolchain. The macOS and native Windows lanes stay
// in ci.yml -- Dagger runs Linux containers only. See
// docs/plans/active/03_dagger-ci.md.
//
// Versions are explicit inputs: the build toolchain comes from
// rust-toolchain.toml and the MSRV from Cargo.toml's rust-version (each still
// declared once), and every image tag and download is pinned below.
package main

import (
	"context"
	"fmt"
	"regexp"
	"strings"

	"dagger/agenttender-ci/internal/dagger"
)

const (
	// The Debian release is part of the image pin, not decoration.
	debian = "bookworm"
	// Must match DUCKDB_VERSION in ci.yml. Neither side verifies a checksum.
	duckdbVersion = "v1.5.2"
	// Several tests chmod a path to 0o000 and expect EACCES, which root
	// ignores, so every check runs as an ordinary user, as on a GitHub runner.
	user = "tender"
	uid  = "1000"
	// CARGO_HOME lives on a shared cache volume, so the registry and cargo's
	// `.package-cache` lock are both on it: concurrent checks then serialise
	// registry writes through that lock instead of racing.
	cargoHome = "/cargo"
)

type AgenttenderCi struct {
	// +private
	Source *dagger.Directory
}

func New(
	// Repository checkout. Build outputs, VCS metadata and CI config are
	// excluded, so none of them can invalidate the cache; `cargo package`
	// needs no VCS because Cargo.toml lists what to include and the check
	// passes --allow-dirty.
	// +defaultPath="/"
	// +ignore=["target", ".git", ".github", ".devenv", ".direnv", "result", "result-*"]
	source *dagger.Directory,
) *AgenttenderCi {
	return &AgenttenderCi{Source: source}
}

var (
	channelRe    = regexp.MustCompile(`(?m)^\s*channel\s*=\s*"([^"]+)"`)
	componentsRe = regexp.MustCompile(`(?m)^\s*components\s*=\s*\[([^\]]*)\]`)
	msrvRe       = regexp.MustCompile(`(?m)^\s*rust-version\s*=\s*"([^"]+)"`)
)

// toolchain reads the pinned channel and components from rust-toolchain.toml.
func (m *AgenttenderCi) toolchain(ctx context.Context) (string, []string, error) {
	toml, err := m.Source.File("rust-toolchain.toml").Contents(ctx)
	if err != nil {
		return "", nil, err
	}
	match := channelRe.FindStringSubmatch(toml)
	if match == nil {
		return "", nil, fmt.Errorf("rust-toolchain.toml: no channel")
	}
	var components []string
	if c := componentsRe.FindStringSubmatch(toml); c != nil {
		for _, part := range strings.Split(c[1], ",") {
			if name := strings.Trim(strings.TrimSpace(part), `"`); name != "" {
				components = append(components, name)
			}
		}
	}
	return match[1], components, nil
}

// msrv reads Cargo.toml's rust-version; "1.85" is checked on "1.85.0".
func (m *AgenttenderCi) msrv(ctx context.Context) (string, error) {
	manifest, err := m.Source.File("Cargo.toml").Contents(ctx)
	if err != nil {
		return "", err
	}
	match := msrvRe.FindStringSubmatch(manifest)
	if match == nil {
		return "", fmt.Errorf("Cargo.toml: no rust-version")
	}
	version := match[1]
	if strings.Count(version, ".") == 1 {
		version += ".0"
	}
	return version, nil
}

// base is the toolchain image with the tools the tests spawn (`ps`, `python3`,
// `bash`, and `shasum` from perl) and an unprivileged user. Everything here is
// independent of the source, so a source edit reuses it.
func base(toolchain string, components, targets []string) *dagger.Container {
	ctr := dag.Container().
		From(fmt.Sprintf("rust:%s-slim-%s", toolchain, debian)).
		WithExec([]string{"sh", "-c", "apt-get update -qq && apt-get install -y -qq --no-install-recommends " +
			"procps python3 perl bash ca-certificates unzip >/dev/null && rm -rf /var/lib/apt/lists/*"}).
		WithExec([]string{"useradd", "--uid", uid, "--create-home", user})
	if len(components) > 0 {
		ctr = ctr.WithExec(append([]string{"rustup", "component", "add", "--toolchain", toolchain}, components...))
	}
	if len(targets) > 0 {
		ctr = ctr.WithExec(append([]string{"rustup", "target", "add", "--toolchain", toolchain}, targets...))
	}
	return ctr
}

// withSource adds the cargo home and, when lane is set, that lane's target
// directory as cache volumes, then the source, as the unprivileged user.
//
// Each lane has its own target volume, so concurrent checks never share a
// build directory across containers; the volume name carries every input that
// changes its contents (lane and toolchain).
func (m *AgenttenderCi) withSource(ctr *dagger.Container, toolchain, lane string) *dagger.Container {
	owner := dagger.ContainerWithMountedCacheOpts{Owner: uid}
	ctr = ctr.
		WithMountedCache(cargoHome, dag.CacheVolume("agenttender-cargo-home"), owner).
		WithEnvVariable("CARGO_HOME", cargoHome).
		WithEnvVariable("CARGO_TERM_COLOR", "always")
	if lane != "" {
		ctr = ctr.
			WithMountedCache("/target", dag.CacheVolume(fmt.Sprintf("agenttender-target-%s-%s", lane, toolchain)), owner).
			WithEnvVariable("CARGO_TARGET_DIR", "/target")
	}
	return ctr.
		WithDirectory("/src", m.Source, dagger.ContainerWithDirectoryOpts{Owner: uid}).
		WithWorkdir("/src").
		WithUser(user)
}

// pinned is the rust-toolchain.toml toolchain, ready to build lane.
func (m *AgenttenderCi) pinned(ctx context.Context, lane string, targets ...string) (*dagger.Container, error) {
	channel, components, err := m.toolchain(ctx)
	if err != nil {
		return nil, err
	}
	return m.withSource(base(channel, components, targets), channel, lane), nil
}

func run(ctx context.Context, ctr *dagger.Container, args ...string) error {
	_, err := ctr.WithExec(args).Sync(ctx)
	return err
}

// Fmt mirrors ci.yml lint/Format.
// +check
func (m *AgenttenderCi) Fmt(ctx context.Context) error {
	ctr, err := m.pinned(ctx, "")
	if err != nil {
		return err
	}
	return run(ctx, ctr, "cargo", "fmt", "--all", "--check")
}

// Clippy mirrors ci.yml lint/Clippy.
// +check
func (m *AgenttenderCi) Clippy(ctx context.Context) error {
	ctr, err := m.pinned(ctx, "clippy")
	if err != nil {
		return err
	}
	return run(ctx, ctr, "cargo", "clippy", "--all-targets", "--locked", "--", "-D", "warnings")
}

// Package mirrors ci.yml lint/Package (locked).
// +check
func (m *AgenttenderCi) Package(ctx context.Context) error {
	ctr, err := m.pinned(ctx, "package")
	if err != nil {
		return err
	}
	return run(ctx, ctr, "cargo", "package", "--locked", "--allow-dirty")
}

// Doc mirrors ci.yml doc: rustdoc with warnings denied.
// +check
func (m *AgenttenderCi) Doc(ctx context.Context) error {
	ctr, err := m.pinned(ctx, "doc")
	if err != nil {
		return err
	}
	return run(ctx, ctr.WithEnvVariable("RUSTDOCFLAGS", "-D warnings"), "cargo", "doc", "--locked", "--no-deps")
}

// Msrv mirrors ci.yml msrv: type-check every target on the advertised
// minimum. `+<version>` overrides rust-toolchain.toml, which is the point.
// +check
func (m *AgenttenderCi) Msrv(ctx context.Context) error {
	version, err := m.msrv(ctx)
	if err != nil {
		return err
	}
	ctr := m.withSource(base(version, nil, nil), version, "msrv")
	return run(ctx, ctr, "cargo", "+"+version, "check", "--locked", "--all-targets")
}

// ClippyWindows mirrors ci.yml clippy-windows.
// +check
func (m *AgenttenderCi) ClippyWindows(ctx context.Context) error {
	targets := []string{"x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"}
	ctr, err := m.pinned(ctx, "clippy-windows", targets...)
	if err != nil {
		return err
	}
	for _, target := range targets {
		if err := run(ctx, ctr, "cargo", "clippy", "--all-targets", "--locked", "--target", target, "--", "-D", "warnings"); err != nil {
			return err
		}
	}
	return nil
}

// Test mirrors ci.yml test (ubuntu-latest): all targets, then doc tests, with
// the pinned DuckDB CLI required so the analytics tests cannot skip.
// +check
func (m *AgenttenderCi) Test(ctx context.Context) error {
	platform, err := dag.DefaultPlatform(ctx)
	if err != nil {
		return err
	}
	// "linux/amd64" -> "amd64", "linux/arm64/v8" -> "arm64": DuckDB's names.
	arch := strings.Split(string(platform), "/")[1]
	duckdb := dag.HTTP(fmt.Sprintf(
		"https://github.com/duckdb/duckdb/releases/download/%s/duckdb_cli-linux-%s.zip", duckdbVersion, arch))

	channel, components, err := m.toolchain(ctx)
	if err != nil {
		return err
	}
	// DuckDB goes in before the source, so a source edit does not redo it.
	ctr := base(channel, components, nil).
		WithMountedFile("/tmp/duckdb.zip", duckdb).
		WithExec([]string{"unzip", "-q", "-o", "/tmp/duckdb.zip", "-d", "/usr/local/bin"}).
		WithExec([]string{"duckdb", "--version"})
	ctr = m.withSource(ctr, channel, "test").
		WithEnvVariable("TENDER_REQUIRE_DUCKDB_TESTS", "1")
	if err := run(ctx, ctr, "cargo", "test", "--locked", "--all-targets", "--no-fail-fast"); err != nil {
		return err
	}
	return run(ctx, ctr, "cargo", "test", "--locked", "--doc", "--no-fail-fast")
}
