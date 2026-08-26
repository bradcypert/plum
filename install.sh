#!/bin/sh
# Downloads a Plum release and puts `plum` on your PATH.
#
#   curl -fsSL https://raw.githubusercontent.com/bradcypert/plum/main/install.sh | sh
#
# Or, having cloned:  ./install.sh
#
# Options, as environment variables:
#   PLUM_VERSION   a tag like `v0.0.4` (default: the latest release)
#   PLUM_PREFIX    where to install (default: $HOME/.local/bin)
#
# **It does not edit your shell configuration.** If the install
# directory is not on your PATH it prints the line to add and stops.
# Printing a line you can read is undoable; rewriting `.zshrc` is not,
# and a script piped from the internet is the worst possible place to
# do something you cannot see.
#
# POSIX sh on purpose: this runs before Plum exists on the machine, and
# possibly before bash does.
set -eu

REPO="bradcypert/plum"
PREFIX="${PLUM_PREFIX:-$HOME/.local/bin}"

die() { printf 'install: %s\n' "$1" >&2; exit 1; }

# --- which archive ---

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
    Linux)                  os_slug="linux" ;;
    Darwin)                 os_slug="macos" ;;
    MINGW*|MSYS*|CYGWIN*)   os_slug="windows" ;;
    *) die "unsupported operating system: $os" ;;
esac

case "$arch" in
    x86_64|amd64)   arch_slug="x86_64" ;;
    arm64|aarch64)  arch_slug="arm64" ;;
    *) die "unsupported architecture: $arch" ;;
esac

slug="${arch_slug}-${os_slug}"

# Only these four are built and tested. Anything else is refused with
# the honest reason rather than a 404 from the download.
case "$slug" in
    x86_64-linux|arm64-linux|arm64-macos|x86_64-macos|x86_64-windows) ;;
    *) die "no published binary for $slug.
     Build one instead -- it needs only clang:
       git clone https://github.com/$REPO && cd plum
       ./bootstrap/from-seed -o plum && ./plum build bootstrap/self_host -o plum" ;;
esac

# --- tools ---

if command -v curl > /dev/null 2>&1; then
    fetch() { curl -fsSL "$1" -o "$2"; }
    fetch_stdout() { curl -fsSL "$1"; }
elif command -v wget > /dev/null 2>&1; then
    fetch() { wget -qO "$2" "$1"; }
    fetch_stdout() { wget -qO - "$1"; }
else
    die "needs curl or wget"
fi

# GNU coreutils and the BSD/macOS tools disagree on the name.
if command -v sha256sum > /dev/null 2>&1; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum > /dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
    sha256() { echo ""; }
fi

# --- which version ---

tag="${PLUM_VERSION:-}"
if [ -z "$tag" ]; then
    # The releases API rather than the `latest/download` redirect: the
    # file name contains the version, so the version has to be known
    # before the URL can be built.
    tag="$(fetch_stdout "https://api.github.com/repos/$REPO/releases/latest" \
        | sed -n 's/.*"tag_name" *: *"\([^"]*\)".*/\1/p' | head -n 1)"
    [ -n "$tag" ] || die "could not determine the latest release"
fi
version="${tag#v}"

name="plum-${version}-${slug}"
url="https://github.com/$REPO/releases/download/$tag/${name}.tar.gz"

printf 'installing plum %s (%s)\n' "$version" "$slug"

# --- download and verify ---

tmp="$(mktemp -d 2>/dev/null || mktemp -d -t pluminstall)"
trap 'rm -rf "$tmp"' EXIT INT TERM

# A 404 here almost always means the platform is supported NOW but was
# not when this release was cut -- the docs and the release move
# independently, and the docs move first. Say that, rather than leaving
# someone staring at a curl error code.
fetch "$url" "$tmp/archive.tar.gz" || die "no ${slug} archive in release ${tag}.
     That platform may have been added after this release was cut. Try
     the newest release explicitly, or build from source:
       PLUM_VERSION=<newer tag> ...
       git clone https://github.com/$REPO && cd plum
       ./bootstrap/from-seed -o plum && ./plum build bootstrap/self_host -o plum
     (url was $url)"

# Verification is not optional when it is possible. A checksum is
# published beside every archive; skipping the check would make this
# script the weakest link in a chain that otherwise has one.
if fetch "${url}.sha256" "$tmp/archive.sha256" 2>/dev/null; then
    want="$(cut -d' ' -f1 < "$tmp/archive.sha256")"
    got="$(sha256 "$tmp/archive.tar.gz")"
    if [ -z "$got" ]; then
        printf 'warning: no sha256 tool found, checksum NOT verified\n' >&2
    elif [ "$want" != "$got" ]; then
        die "checksum mismatch
     expected $want
     got      $got"
    else
        printf 'checksum ok\n'
    fi
else
    printf 'warning: no published checksum, integrity NOT verified\n' >&2
fi

tar -xzf "$tmp/archive.tar.gz" -C "$tmp"

exe="plum"
[ "$os_slug" = "windows" ] && exe="plum.exe"
[ -f "$tmp/$name/$exe" ] || die "the archive did not contain $exe"

# --- install ---

mkdir -p "$PREFIX" || die "could not create $PREFIX"
cp "$tmp/$name/$exe" "$PREFIX/$exe" || die "could not write to $PREFIX"
chmod +x "$PREFIX/$exe"

printf 'installed %s\n' "$PREFIX/$exe"

# Prove it runs before claiming success. A binary for the wrong
# architecture copies perfectly well and then fails on first use.
if ! "$PREFIX/$exe" version > /dev/null 2>&1; then
    die "installed, but $PREFIX/$exe does not run -- wrong architecture?"
fi
printf '%s\n' "$("$PREFIX/$exe" version)"

# --- PATH ---

case ":${PATH}:" in
    *":${PREFIX}:"*)
        printf '\nready. try:  plum new hello && plum run hello\n' ;;
    *)
        printf '\n%s is not on your PATH. Add it:\n\n' "$PREFIX"
        printf '    export PATH="%s:$PATH"\n\n' "$PREFIX"
        printf 'then:  plum new hello && plum run hello\n' ;;
esac

printf '\nplum needs clang on your PATH to compile -- it shells out to it\nto assemble and link. Nothing else is required.\n'
