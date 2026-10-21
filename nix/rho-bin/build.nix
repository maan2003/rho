# `rho-bin-build <build|release> [git-ref]`: builds the binaries of the
# `rho` package for `system`, cross-compiled from x86_64-linux, as an
# archive `default.nix` turns back into `rho` on the machine. Built with none of the `rho` package's
# store paths compiled in; the binaries find their runtime beside them.
#
# State (a clone and its cargo target directory, so rebuilds are
# incremental) lives in $STATE_DIRECTORY, default ~/.local/state/rho-bin.
# The clone fetches from $RHO_BIN_REPO, default GitHub. `release` uploads
# with the rclone config in $RHO_BIN_RCLONE (an `[r2]` remote), default
# ~/.config/rclone/rclone.conf, and writes the pin to print.
{
  pkgs,
  system,
  fenix,
  libraries,
  webrtcPrebuilts,
  skills,
  # Rho's patched Nix built for `system`: rho-devshell-builder links its C
  # API, and the archive carries its libraries at share/rho/lib.
  nix,
}:

let
  pkgsTarget = import pkgs.path { inherit system; };
  target = pkgsTarget.stdenv.hostPlatform.config;
  native = system == pkgs.stdenv.hostPlatform.system;
  # Nightly: the workspace's .cargo/config.toml uses unstable cargo keys.
  toolchain = fenix.combine [
    fenix.latest.cargo
    fenix.latest.rustc
    fenix.targets.${target}.latest.rust-std
  ];
  crossCc =
    if native then
      pkgs.stdenv.cc
    else
      (import pkgs.path {
        localSystem = pkgs.stdenv.hostPlatform.system;
        crossSystem = system;
      }).stdenv.cc;
  cargoTarget = pkgs.lib.toUpper (builtins.replaceStrings [ "-" ] [ "_" ] target);
  ccTarget = builtins.replaceStrings [ "-" ] [ "_" ] target;
  # The target's own packages, so they come from cache.nixos.org: the
  # compiler needs only their headers, .pc files and shared objects.
  # libpython is the one `rho-bin` patches the notebook against.
  python = pkgsTarget.python3;
  linked = libraries pkgsTarget ++ [ python ];
  # The workspace links x86_64 with mold (.cargo/config.toml); a target's
  # flags replace that file's, so they say it again.
  linkerFlags = pkgs.lib.optionalString (pkgsTarget.stdenv.hostPlatform.isx86_64) "-Clink-arg=-fuse-ld=mold";
  pkgConfigPath = pkgs.lib.concatStringsSep ":" (
    map (lib: "${pkgs.lib.getDev lib}/lib/pkgconfig") linked
    ++ map (lib: "${pkgs.lib.getDev lib}/share/pkgconfig") linked
  );
  # Every shared library Nix's libraries load, but the C runtime the
  # machine supplies itself: glibc, and GCC's libstdc++ and libgcc_s.
  # Its libraries alone: the whole of `nix` would build its manual, which
  # needs a Nix for the build machine as well.
  nixLibs = pkgs.lib.attrValues nix.libs;
  nixLibraries = pkgs.closureInfo { rootPaths = nixLibs; };
  # Crates that pass bare `-l` flags rely on the linker search path.
  libSearchFlags = pkgs.lib.concatMapStringsSep " " (
    lib: "-L native=${pkgs.lib.getLib lib}/lib"
  ) (linked ++ nixLibs);
  # The `rho` package's binaries.
  packages = [
    "rho-cli"
    "rho-gui"
    "rho-agent-host"
    "rho-inference"
    "rho-shell"
    "rho-devshell-daemon"
    "rho-devshell-builder"
    "git-remote-octo"
  ];
in
pkgs.writeShellApplication {
  name = "rho-bin-build";
  runtimeInputs = [
    toolchain
    crossCc
    # Build scripts and proc macros, linked as .cargo/config.toml says.
    pkgs.stdenv.cc
    pkgs.mold
  ]
  ++ (with pkgs; [
    llvmPackages.clang-unwrapped
    llvmPackages.llvm
    cmake
    coreutils
    findutils
    git
    gawk
    gnugrep
    gnumake
    gnused
    gnutar
    perl
    pkg-config
    which
    protobuf
    python3
    rclone
    util-linux
    zstd
  ]);
  text = ''
    action="''${1:-}"
    ref="''${2:-main}"
    case "$action" in
      build|release) ;;
      *)
        echo "usage: rho-bin-build <build|release> [git-ref]" >&2
        exit 2
        ;;
    esac

    state="''${STATE_DIRECTORY:-$HOME/.local/state/rho-bin}"
    mkdir -p "$state"
    exec 9>"$state/build.lock"
    if ! flock -n 9; then
      echo "another rho-bin build is running" >&2
      exit 1
    fi

    repo="$state/repo"
    if [[ ! -d "$repo/.git" ]]; then
      git clone "''${RHO_BIN_REPO:-https://github.com/maan2003/rho}" "$repo"
    fi
    git -C "$repo" fetch "''${RHO_BIN_REPO:-origin}" "+refs/heads/*:refs/remotes/origin/*"
    if git -C "$repo" rev-parse --verify --quiet "refs/remotes/origin/$ref" > /dev/null; then
      git -C "$repo" checkout --detach "refs/remotes/origin/$ref"
    else
      git -C "$repo" checkout --detach "$ref"
    fi
    revision="$(git -C "$repo" rev-parse HEAD)"
    version="$(git -C "$repo" show -s --format=%cd --date=format:%Y%m%d HEAD)-g''${revision:0:12}"
    echo "rho-bin $version, ${target}"

    # None of the `rho` package's store paths, and nothing of a dev shell's
    # Python, whose sysconfig names would send PyO3 looking for the wrong one.
    unset RHO_AGENT_BASE RHO_PYTHON_SITE_PACKAGES RHO_FIND_BIN RHO_SHARED_CARGO_BIN \
      RHO_BUNDLED_SKILLS_DIR OCTO_REMOTE_HTTP CARGO_BUILD_TARGET RUSTFLAGS \
      PYO3_PYTHON PYTHONPATH _PYTHON_SYSCONFIGDATA_NAME _PYTHON_HOST_PLATFORM
    export CARGO_TARGET_DIR="$state/target"
    export CARGO_TARGET_${cargoTarget}_LINKER=${crossCc}/bin/${crossCc.targetPrefix}cc
    export CC_${ccTarget}=${crossCc}/bin/${crossCc.targetPrefix}cc
    export CXX_${ccTarget}=${crossCc}/bin/${crossCc.targetPrefix}c++
    export AR_${ccTarget}=${crossCc.bintools.bintools}/bin/${crossCc.targetPrefix}ar
    export CARGO_TARGET_${cargoTarget}_RUSTFLAGS="--cfg tokio_unstable -Cforce-frame-pointers=yes ${linkerFlags} ${libSearchFlags}"
    export PKG_CONFIG_ALLOW_CROSS=1
    # Nix's .pc files name its own dependencies' (bdw-gc and more), which
    # its dev output propagates rather than carries.
    nix_pc=""
    declare -A seen=()
    propagated() {
      local path
      for path in "$@"; do
        [[ -n "''${seen[$path]-}" ]] && continue
        seen[$path]=1
        for dir in "$path/lib/pkgconfig" "$path/share/pkgconfig"; do
          [[ -d "$dir" ]] && nix_pc="$nix_pc:$dir"
        done
        if [[ -f "$path/nix-support/propagated-build-inputs" ]]; then
          # shellcheck disable=SC2046
          propagated $(cat "$path/nix-support/propagated-build-inputs")
        fi
      done
    }
    propagated ${pkgs.lib.concatMapStringsSep " " pkgs.lib.getDev nixLibs}
    export PKG_CONFIG_PATH_${ccTarget}=${pkgConfigPath}$nix_pc
    export PROTOC=${pkgs.protobuf}/bin/protoc
    export LIBCLANG_PATH=${pkgs.libclang.lib}/lib
    export BINDGEN_EXTRA_CLANG_ARGS_${ccTarget}="--target=${target} --sysroot=${
      crossCc.libc.dev or crossCc.libc
    }"
    export LK_CUSTOM_WEBRTC=${webrtcPrebuilts.${system}}
    export PYO3_CROSS_LIB_DIR=${python}/lib
    export PYO3_CROSS_PYTHON_VERSION=${python.pythonVersion}
    # aarch64 kernels use 4 KiB or 16 KiB pages; 16 KiB serves both.
    export JEMALLOC_SYS_WITH_LG_PAGE=${if pkgsTarget.stdenv.hostPlatform.isAarch64 then "14" else "12"}

    cd "$repo"
    cargo build --release --locked --target ${target} ${
      pkgs.lib.concatMapStringsSep " " (p: "-p ${p}") packages
    }

    stage="$state/stage"
    rm -rf "$stage"
    mkdir -p "$stage/bin" "$stage/share/rho/skills"
    binaries="$CARGO_TARGET_DIR/${target}/release"
    for binary in rho rho-gui rho-agent-host rho-agent-worker rho-shell rho-pager \
      rho-devshell-daemon rho-devshell-builder git-remote-octo; do
      llvm-objcopy --strip-debug "$binaries/$binary" "$stage/bin/$binary"
    done
    mkdir -p "$stage/share/rho/lib"
    while read -r path; do
      case "$path" in
        *-glibc-*|*-gcc-*|*-libgcc*) continue ;;
      esac
      if [[ -d "$path/lib" ]]; then
        find "$path/lib" -maxdepth 1 -name '*.so*' \( -type f -o -type l \) \
          -exec cp --no-dereference --no-clobber -t "$stage/share/rho/lib" {} +
      fi
    done < ${nixLibraries}/store-paths
    chmod -R u+w "$stage/share/rho/lib"
    for skill in ${pkgs.lib.concatStringsSep " " skills}; do
      cp -r "$repo/.agents/skills/$skill" "$stage/share/rho/skills/$skill"
    done

    archive="$state/rho-bin_''${version}_${system}.tar.zst"
    # Deterministic, so a rebuild of a revision reproduces its pin.
    tar --create --file - --directory "$stage" \
      --sort=name --owner=0 --group=0 --numeric-owner \
      --mtime="@$(git -C "$repo" show -s --format=%ct HEAD)" \
      bin share \
      | zstd -12 -T16 --long=27 -o "$archive" --force
    echo "archive: $archive"
    if [[ "$action" == build ]]; then
      exit 0
    fi

    rclone_config="''${RHO_BIN_RCLONE:-$HOME/.config/rclone/rclone.conf}"
    key="rho-bin/$version/${system}/$(basename "$archive")"
    # Published objects are immutable: a pin is the hash of the first upload.
    if rclone --config "$rclone_config" lsf "r2:build-data/$key" | grep -q .; then
      echo "refusing to overwrite already published $key" >&2
      exit 1
    fi
    rclone --config "$rclone_config" copyto "$archive" "r2:build-data/$key" \
      --s3-no-check-bucket \
      --header-upload "Cache-Control: no-cache" \
      --header-upload "Content-Type: application/zstd"
    hash="$(python3 -c 'import base64, hashlib, sys; print("sha256-" + base64.b64encode(hashlib.file_digest(open(sys.argv[1], "rb"), "sha256").digest()).decode())' "$archive")"
    cat <<EOF
    published $key; in nix/rho-bin/pin.json:
    "${system}": {
      "version": "$version",
      "url": "https://pub-c02578899b644007b0b5aae08b7e73b3.r2.dev/$key",
      "hash": "$hash"
    }
    EOF
  '';
}
