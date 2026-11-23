#!/usr/bin/env bash
# Builds the NSS that Firefox 160 ships, for netweir-core's `http3` feature.
#
#   scripts/nss.sh [TARGET]
#
# TARGET is a Rust target triple, the host's by default. The build goes in
# target/nss/TARGET/, and the script ends by printing the two variables
# cargo needs (and, on GitHub Actions, adds them to $GITHUB_ENV):
#
#   NSS_DIR=.../nss-3.132/nss  NSS_PREBUILT=1
#
# Needs bash, curl, python3 with gyp-next, and ninja (pip install
# gyp-next ninja); on Windows, MSYS2's bash and an MSVC environment.
#
# The NSS release is the one Firefox 160.0.1 builds with
# (security/nss/TAG-INFO: NSS_3_132_RTM). The tarball bundles NSPR 4.39,
# where Firefox has 4.40; NSPR is NSS's portability layer and puts nothing
# on the wire.
#
# nss-rs links NSS's static libraries in debug builds and on Windows, and
# the shared ones (nss3, ssl3, ...) in release builds elsewhere. A wheel
# can't carry those, so on Linux and macOS the shared libraries are
# deleted and archives take their names: libnss3.a holds every library
# nss-rs links statically, and the other names are empty.

set -euo pipefail

version=3.132
nspr=4.39
sha256=c220ea83378e55f2e4d7f7d83477756d622f79b5b91d1ff4157d5e3f494d2b49
url="https://ftp.mozilla.org/pub/security/nss/releases/NSS_${version//./_}_RTM/src/nss-$version-with-nspr-$nspr.tar.gz"

root=$(cd "$(dirname "$0")/.." && pwd)
target=${1:-$(rustc -vV | sed -n 's/^host: //p')}
out="$root/target/nss/$target"
src="$out/nss-$version"
lib="$src/dist/Release/lib"

case "$target" in
  x86_64-*) arch=x64 ;;
  aarch64-*) arch=arm64 ;;
  *) echo "scripts/nss.sh: no NSS build for $target" >&2; exit 1 ;;
esac

if [ ! -f "$out/done" ]; then
  mkdir -p "$out"
  tarball="$out/nss.tar.gz"
  [ -f "$tarball" ] || curl --fail --location --silent --show-error -o "$tarball" "$url"
  if command -v sha256sum >/dev/null; then
    got=$(sha256sum "$tarball" | cut -d' ' -f1)
  else
    got=$(shasum -a 256 "$tarball" | cut -d' ' -f1)
  fi
  if [ "$got" != "$sha256" ]; then
    echo "scripts/nss.sh: $tarball has SHA-256 $got, expected $sha256" >&2
    rm -f "$tarball"
    exit 1
  fi
  rm -rf "$src"
  tar xzf "$tarball" -C "$out"

  # The options nss-rs builds NSS with, plus the target.
  flags=(-Ddisable_tests=1 -Ddisable_dbm=1 -Ddisable_libpkix=1 -Ddisable_ckbi=1
    -Ddisable_fips=1 --opt --static -t "$arch")
  host=$(uname -m)
  if [ "$(uname -s)" = Darwin ] && [ "$arch" = x64 ] && [ "$host" = arm64 ]; then
    # Apple's compilers build for the architecture they run as, so the
    # x86_64 build runs under Rosetta.
    (cd "$src/nss" && arch -x86_64 /bin/bash ./build.sh "${flags[@]}")
  else
    (cd "$src/nss" && ./build.sh "${flags[@]}")
  fi

  # NSS's own cryptography (freebl) defines a few names BoringSSL, which
  # netweir links for TCP, defines too (SHA256_Update, HMAC_Init, ...),
  # and NSS's SQLite every name rusqlite's does; with both in one binary
  # each would call the other's. Their symbols get a prefix, references
  # to them throughout NSS with them, all but FREEBL_GetVector, the one
  # nss-rs calls. (On macOS NSS uses the system's SQLite and has none.)
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*) a_pre=""; a_ext=lib ;;
    *) a_pre=lib; a_ext=a ;;
  esac
  case "$(uname -s)" in Darwin) under=_ ;; *) under= ;; esac
  # GNU objcopy on Linux; elsewhere the LLVM one every Rust toolchain
  # carries, beside the LLVM library it loads.
  if [ "$(uname -s)" = Linux ]; then
    objcopy() { command objcopy "$@"; }
  else
    sysroot=$(rustc --print sysroot)
    host_triple=$(rustc -vV | sed -n 's/^host: //p')
    objcopy() {
      PATH="$sysroot/bin:$PATH" DYLD_LIBRARY_PATH="$sysroot/lib" \
        "$sysroot/lib/rustlib/$host_triple/bin/rust-objcopy" "$@"
    }
  fi
  # The global symbols an archive defines.
  defined() {
    case "$(uname -s)" in
      MINGW* | MSYS* | CYGWIN*)
        dumpbin -nologo -symbols "$1" | awk '/External/ && !/UNDEF/ { print $NF }' ;;
      *) nm -g --defined-only "$1" 2>/dev/null | awk 'NF == 3 { print $3 }' ;;
    esac
  }
  freebl=(freebl_static gcm hw-acc-crypto-avx hw-acc-crypto-avx2 sha-x86_c_lib armv8_c_lib
    crux pqcwrap_static ghash-aes-x86_c_lib ghash-aes-aarch64_c_lib intel-gcm-wrap_c_lib
    aarch64-gcm-wrap_c_lib sqlite)
  map="$out/freebl-symbols.txt"
  # NSPR's own names stay as they are, should freebl carry a copy of any:
  # BoringSSL defines no PR_ or PL_ names, so none of them clash, and on
  # Windows plds reaches them through dllimport names the map can't see.
  nspr_names="$out/nspr-symbols.txt"
  for f in nspr4 plc4 plds4 nspr4_s plc4_s plds4_s; do
    [ ! -f "$lib/$a_pre$f.$a_ext" ] || defined "$lib/$a_pre$f.$a_ext"
  done | tr -d '\r' | sort -u > "$nspr_names"
  for f in "${freebl[@]}"; do
    [ ! -f "$lib/$a_pre$f.$a_ext" ] || defined "$lib/$a_pre$f.$a_ext"
  done | tr -d '\r' | sort -u | grep -v "^${under}FREEBL_GetVector\$" \
    | { if [ -s "$nspr_names" ]; then grep -vxFf "$nspr_names"; else cat; fi; } \
    | awk -v u="$under" '{
        if (u != "" && substr($1, 1, 1) == u) print $1, u "netweir_nss_" substr($1, 2)
        else print $1, "netweir_nss_" $1
      }' > "$map"
  [ -s "$map" ] || { echo "scripts/nss.sh: no freebl symbols found" >&2; exit 1; }
  for a in "$lib"/*."$a_ext"; do
    # On Windows the directory also holds the import libraries for NSS's
    # DLLs, whose short import members objcopy can't rewrite. Nothing
    # links them, so they're left as they are.
    case "$(uname -s)" in
      MINGW* | MSYS* | CYGWIN*)
        if dumpbin -nologo -linkermember:1 "$a" | grep -q __IMPORT_DESCRIPTOR_; then
          echo "scripts/nss.sh: $(basename "$a") is an import library, left alone"
          continue
        fi ;;
    esac
    echo "scripts/nss.sh: prefixing freebl symbols in $(basename "$a")"
    objcopy --redefine-syms="$map" "$a"
  done

  # An archive with no members, which every linker takes as nothing.
  empty() { printf '!<arch>\n' > "$1"; }
  case "$(uname -s)" in
    MINGW* | MSYS* | CYGWIN*)
      # nss-rs links NSPR's static libraries on Windows as libnspr4,
      # libplc4 and libplds4; NSPR's build names them nspr4_s and so on.
      for name in nspr4 plc4 plds4; do
        cp "$lib/${name}_s.lib" "$lib/lib$name.lib"
      done
      ;;
    *)
      # What nss-rs links statically (its build.rs, static_link), as one
      # archive in place of the shared libraries.
      parts=(certdb certhi cryptohi freebl_static gcm nss_static nssb nssdev nsspki
        nssutil pk11wrap_static softokn_static ssl pqcwrap_static crux sqlite)
      case "$arch" in
        arm64) parts+=(armv8_c_lib ghash-aes-aarch64_c_lib aarch64-gcm-wrap_c_lib) ;;
        x64) parts+=(ghash-aes-x86_c_lib sha-x86_c_lib hw-acc-crypto-avx
          hw-acc-crypto-avx2 intel-gcm-wrap_c_lib) ;;
      esac
      archives=()
      for p in "${parts[@]}"; do
        [ ! -f "$lib/lib$p.a" ] || archives+=("$lib/lib$p.a")
      done
      if [ "$(uname -s)" = Darwin ]; then
        libtool -static -no_warning_for_no_symbols -o "$lib/libnss3.a" "${archives[@]}"
      else
        { echo "create $lib/libnss3.a"
          for a in "${archives[@]}"; do echo "addlib $a"; done
          echo save; echo end; } | ar -M
      fi
      for name in ssl3 nssutil3 freebl3; do empty "$lib/lib$name.a"; done
      rm -f "$lib"/*.so "$lib"/*.dylib "$lib"/*.chk "$lib"/*.TOC
      ;;
  esac
  touch "$out/done"
fi

nss_dir="$src/nss"
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) nss_dir=$(cygpath -m "$nss_dir") ;;
esac
echo "NSS_DIR=$nss_dir"
echo "NSS_PREBUILT=1"
if [ -n "${GITHUB_ENV:-}" ]; then
  { echo "NSS_DIR=$nss_dir"; echo "NSS_PREBUILT=1"; } >> "$GITHUB_ENV"
fi
