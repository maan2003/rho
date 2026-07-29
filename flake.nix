{
  description = "rho";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    flake-utils.url = "github:numtide/flake-utils";
    flakebox = {
      url = "github:rustshop/flakebox?rev=cf89db7a3ac6b1431693d17276225ba352e48a5c";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    public-skills = {
      url = "github:maan2003/public-skills";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    selfci = {
      url = "git+https://radicle.dpc.pw/z2tDzYbAXxTQEKTGFVwiJPajkbeDU.git";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
      # TODO: temporarily broken because of wild 0.9.0 hackery
      # inputs.flakebox.follows = "flakebox";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      flakebox,
      public-skills,
      selfci,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [
            flakebox.overlays.default
          ];
        };

        projectName = "rho";
        octoGit = pkgs.git.overrideAttrs (old: {
          patches = (old.patches or [ ]) ++ [
            ./nix/patches/git-http-unix-socket.patch
          ];
        });
        rustyV8Archives = {
          x86_64-linux = pkgs.fetchurl {
            url = "https://github.com/denoland/rusty_v8/releases/download/v149.4.0/librusty_v8_simdutf_release_x86_64-unknown-linux-gnu.a.gz";
            hash = "sha256-qjDxmLbnviGI32SY+VBTxMBS8hIDegHywxQU16yoS1M=";
          };
          aarch64-linux = pkgs.fetchurl {
            url = "https://github.com/denoland/rusty_v8/releases/download/v149.4.0/librusty_v8_simdutf_release_aarch64-unknown-linux-gnu.a.gz";
            hash = "sha256-VPd5M2+oXRbqeVD4LTuLMTJq4JushNWXY9ssXOqgCUw=";
          };
          x86_64-darwin = pkgs.fetchurl {
            url = "https://github.com/denoland/rusty_v8/releases/download/v149.4.0/librusty_v8_simdutf_release_x86_64-apple-darwin.a.gz";
            hash = "sha256-GIl8QmcKyYhVB7bU02xd+7nEzBNjHMn8VqbohnS7Nkk=";
          };
          aarch64-darwin = pkgs.fetchurl {
            url = "https://github.com/denoland/rusty_v8/releases/download/v149.4.0/librusty_v8_simdutf_release_aarch64-apple-darwin.a.gz";
            hash = "sha256-1PPs3PF2RqmlcGbUTda2rvrChG+wb9RRuAiwOLiuvnM=";
          };
        };
        rustyV8Archive = rustyV8Archives.${system};
        webrtcPrebuilts = {
          x86_64-linux = pkgs.fetchzip {
            url = "https://github.com/zed-industries/livekit-rust-sdks/releases/download/webrtc-0001d84-4/webrtc-linux-x64-release.zip";
            sha256 = "0hlv1p6fi1lgfdyq8q49gghbqbqnq50icrw7i25g2qwqmpf2jyi7";
          };
          aarch64-linux = pkgs.fetchzip {
            url = "https://github.com/zed-industries/livekit-rust-sdks/releases/download/webrtc-0001d84-4/webrtc-linux-arm64-release.zip";
            sha256 = "07dvljd51w6gaw0s9wy853fgzdszh2jb393x5rcdmvvlck8mbf01";
          };
          x86_64-darwin = pkgs.fetchzip {
            url = "https://github.com/zed-industries/livekit-rust-sdks/releases/download/webrtc-0001d84-4/webrtc-mac-x64-release.zip";
            sha256 = "01zi72i6nvk0lx92g747160bxv4lla908z9y9f8v9i2mwiqky8qd";
          };
          aarch64-darwin = pkgs.fetchzip {
            url = "https://github.com/zed-industries/livekit-rust-sdks/releases/download/webrtc-0001d84-4/webrtc-mac-arm64-release.zip";
            sha256 = "0mlcgqyd9b29cp70c73xwkaydid7n4i9xd3zpcx10ccrlnlzc3ds";
          };
        };
        webrtcPrebuilt = webrtcPrebuilts.${system};
        selfciPkg = selfci.packages.${system}.default;
        selfciMq = selfci.packages.${system}.mq;

        flakeboxLib = flakebox.lib.mkLib pkgs {
          config = {
            github.ci.buildOutputs = [ ".#ci.workspace" ];
            just.importPaths = [ "justfile.custom.just" ];
            just.rules.watch.enable = false;
            toolchain.components = [
              "rustc"
              "cargo"
              "clippy"
              "rust-analyzer"
              "rust-src"
              "llvm-tools"
            ];
          };
        };

        muslToolchains =
          flakeboxLib.mkStdToolchains { }
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            x86_64-musl = flakeboxLib.mkFenixToolchain {
              defaultTarget = "x86_64-unknown-linux-musl";
              stdenv = pkgs.pkgsCross.musl64.stdenv;
              targets = {
                x86_64-musl = flakeboxLib.mkTarget {
                  target = "x86_64-unknown-linux-musl";
                  canUseMold = false;
                  canUseWild = false;
                  args = {
                    nativeBuildInputs = [ pkgs.stdenv.cc ];
                    CC = "${pkgs.stdenv.cc}/bin/cc";
                    CXX = "${pkgs.stdenv.cc}/bin/c++";
                    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${pkgs.pkgsCross.musl64.stdenv.cc}/bin/x86_64-unknown-linux-musl-gcc";
                    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "--cfg tokio_unstable";
                  };
                };
              };
            };
          };

        buildPaths = [
          "Cargo.toml"
          "Cargo.lock"
          "README.md"
          ".config/nextest.toml"
          "crates"
          "vendor"
        ];

        buildSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = projectName;
            path = ./.;
          };
          paths = buildPaths;
        };

        # The web UI is its own cargo workspace targeting wasm32, built with
        # trunk into a static bundle. Reproducible `nix build .#webui` output
        # is the deployable artifact: anyone can rebuild and byte-compare
        # against the hosted bundle. The source still needs the root
        # workspace manifest and `crates`/`vendor` trees because the webui
        # depends on `rho-iroh-auth` (workspace-inherited fields) and patches
        # in the vendored `iroh`/`noq` forks.
        webuiSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = projectName;
            path = ./.;
          };
          paths = buildPaths ++ [
            "webui"
            "crates/json-stream"
            "crates/prefix-id"
            "crates/rho-core"
            "crates/rho-iroh-auth"
            "crates/rho-registry"
            "crates/rho-shell-proto"
            "crates/rho-ui-proto"
            "crates/rho-workspaces-types"
            "crates/senax-encoder"
          ];
        };

        # Trunk requires the wasm-bindgen CLI version to exactly match the
        # `wasm-bindgen` crate in webui/Cargo.lock.
        wasmBindgenCli = pkgs.buildWasmBindgenCli rec {
          src = pkgs.fetchCrate {
            pname = "wasm-bindgen-cli";
            version = "0.2.126";
            hash = "sha256-H6Is3fiZVxZCfOMWK5dWMSrtn50VGv0sfdnsT+cTtyk=";
          };
          cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
            inherit src;
            inherit (src) pname version;
            hash = "sha256-VucqkXbCi4qtQzY/HrXiDnbSURsagPsdNVMn1Tw3UiY=";
          };
        };

        # Adds CSP hash sources for the inline scripts trunk injects into
        # index.html; the meta-tag policy would otherwise block the wasm
        # bootstrap on static hosts, where per-response nonces are impossible.
        webuiCspHash = pkgs.writeText "webui-csp-hash.py" ''
          import base64
          import hashlib
          import re
          import sys

          path = sys.argv[1]
          with open(path) as f:
              html = f.read()
          hashes = []
          for m in re.finditer(r"<script(?![^>]*\bsrc=)[^>]*>(.*?)</script>", html, re.DOTALL):
              digest = base64.b64encode(hashlib.sha256(m.group(1).encode()).digest()).decode()
              hashes.append("'sha256-" + digest + "'")
          assert hashes, "no inline scripts found in index.html"
          new, count = re.subn(r"script-src 'self'", "script-src 'self' " + " ".join(hashes), html)
          assert count == 1, "expected one script-src directive, found %d" % count
          with open(path, "w") as f:
              f.write(new)
        '';

        webui = pkgs.rustPlatform.buildRustPackage {
          pname = "rho-webui";
          version = "0.1.0";
          src = webuiSrc;
          sourceRoot = "source/webui";
          cargoLock.lockFile = ./webui/Cargo.lock;
          nativeBuildInputs = [
            pkgs.trunk
            wasmBindgenCli
            pkgs.binaryen
            pkgs.lld
            pkgs.python3
          ];
          # `ring` compiles C for wasm32; the wrapped clang injects host
          # flags that produce unlinkable objects.
          env.CC_wasm32_unknown_unknown = "${pkgs.llvmPackages.clang-unwrapped}/bin/clang";
          buildPhase = ''
            runHook preBuild
            export TRUNK_OFFLINE=true
            export TRUNK_SKIP_VERSION_CHECK=true
            export XDG_CACHE_HOME=$TMPDIR/cache
            # Relative public URL: GitHub Pages serves project sites under a
            # /<repo>/ subpath.
            trunk build --release --dist dist --public-url ./
            # Allow the trunk-injected inline bootstrap script through the
            # CSP by hash; static hosts cannot use nonces.
            python3 ${webuiCspHash} dist/index.html
            runHook postBuild
          '';
          installPhase = ''
            runHook preInstall
            cp -r dist $out
            runHook postInstall
          '';
          doCheck = false;
        };

        guiNativeBuildInputs = [
          pkgs.clang
          pkgs.cmake
          pkgs.pkg-config
          pkgs.protobuf
        ];
        guiBuildInputs = [
          pkgs.alsa-lib
          pkgs.fontconfig
          pkgs.freetype
          pkgs.glib
          pkgs.libdrm
          pkgs.libgbm
          pkgs.libglvnd
          pkgs.libva
          pkgs.libxkbcommon
          pkgs.openssl
          pkgs.vulkan-loader
          pkgs.wayland
        ];
        guiLibraryPath = pkgs.lib.makeLibraryPath guiBuildInputs;
        zedSrc = ./vendor/zed;
        zedVendorManifest = pkgs.writeText "zed-vendor-Cargo.toml" ''
          [package]
          name = "zed"
          version = "1.11.0"
          edition = "2024"

          [lib]
          path = "lib.rs"
        '';
        zedVendorLib = pkgs.writeText "zed-vendor-lib.rs" "";
        zedVendorChecksum = pkgs.writeText "zed-vendor-checksum.json" ''
          {"files":{},"package":null}
        '';

        multiBuild = (flakeboxLib.craneMultiBuild { toolchains = muslToolchains; }) (
          craneLib':
          let
            craneLibBase = craneLib'.overrideArgs {
              pname = projectName;
              src = buildSrc;
              nativeBuildInputs = guiNativeBuildInputs;
              buildInputs = guiBuildInputs;
              env.RUSTDOCFLAGS = "-D warnings";
              env.PROTOC = "${pkgs.protobuf}/bin/protoc";
              env.OCTO_REMOTE_HTTP = "${octoGit}/libexec/git-core/git-remote-http";
              env.RHO_WAYLAND_SWAY = "${pkgs.sway}/bin/sway";
              env.RHO_WAYLAND_SWAYMSG = "${pkgs.sway}/bin/swaymsg";
              env.RHO_WAYLAND_GRIM = "${pkgs.grim}/bin/grim";
              env.RHO_WAYLAND_WTYPE = "${pkgs.wtype}/bin/wtype";
              env.RHO_WAYLAND_VK_DRIVER_FILES = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json";
              env.LK_CUSTOM_WEBRTC = webrtcPrebuilt;
              env.RUSTY_V8_ARCHIVE = rustyV8Archive;
              postPatch = ''
                # Brush denies warnings, but the root lockfile can select a
                # newer Clap which deprecates attributes used by Brush.
                substituteInPlace vendor/brush/Cargo.toml \
                  --replace-fail 'warnings = { level = "deny" }' \
                    'warnings = { level = "warn" }'
              '';
              CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "--cfg tokio_unstable";
              CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "--cfg tokio_unstable";
            };
            cargoVendorDirBase = craneLibBase.vendorCargoDeps { };
            cargoVendorDir = pkgs.runCommand "rho-cargo-vendor-deps" { } ''
              cp -aL ${cargoVendorDirBase} $out
              chmod -R u+w $out
              substituteInPlace $out/config.toml \
                --replace-fail ${cargoVendorDirBase} $out

              # The Zed `assets` crate embeds files from `../../assets`. Crane's
              # vendoring splits git workspaces into per-crate directories, so
              # provide the full-repo asset directory at the relative path the
              # crate expects.
              ln -s ${zedSrc}/assets $out/assets

              # `extension_host` likewise reads the sibling extension API WIT
              # definitions from its build script. Git dependencies live one
              # directory below their source hash in Crane's vendor tree.
              for extensionHost in $out/*/extension_host-*; do
                extensionApi="$(dirname "$extensionHost")/extension_api"
                mkdir "$extensionApi"
                ln -s ${zedSrc}/crates/extension_api/wit "$extensionApi/wit"
              done

              # `remote_server` embeds the Zed package version from the sibling
              # `zed` manifest. Supply a standalone manifest so Cargo can also
              # scan the reconstructed vendor source without workspace context.
              for remoteServer in $out/*/remote_server-*; do
                zedPackage="$(dirname "$remoteServer")/zed"
                mkdir "$zedPackage"
                ln -s ${zedVendorManifest} "$zedPackage/Cargo.toml"
                ln -s ${zedVendorLib} "$zedPackage/lib.rs"
                ln -s ${zedVendorChecksum} "$zedPackage/.cargo-checksum.json"
              done
            '';
            craneLib = craneLibBase.overrideArgs {
              inherit cargoVendorDir;
            };
          in
          rec {
            workspaceDeps = craneLib.buildWorkspaceDepsOnly {
              # Crane stubs every local package while caching workspace
              # dependencies. The patched noq crates are dependencies of iroh,
              # so they must retain their implementations in the dummy source.
              extraDummyScript = ''
                rm -rf $out/vendor/brush $out/vendor/noq
                cp -r --no-preserve=mode,ownership ${buildSrc}/vendor/brush $out/vendor/brush
                cp -r --no-preserve=mode,ownership ${buildSrc}/vendor/noq $out/vendor/noq
              '';
            };

            workspace = craneLib.buildWorkspace {
              cargoArtifacts = workspaceDeps;
            };

            package = craneLib.buildPackage {
              cargoArtifacts = workspaceDeps;
              cargoExtraArgs = "-p rho-cli -p rho-daemon -p rho-shell -p git-remote-octo -p jj-cli";
              doCheck = false;
              env.RHO_BUNDLED_SKILLS_DIR = "${builtins.placeholder "out"}/share/rho/skills";
              postInstall = ''
                install -Dm755 target/release/jj $out/bin/jj
                mkdir -p $out/share/rho/skills
                cp -r ${./.agents/skills/github-workflow} $out/share/rho/skills/github-workflow
                cp -r ${./.agents/skills/delegate-engineering} \
                  $out/share/rho/skills/delegate-engineering
                cp -r ${./.agents/skills/rho-wayland} $out/share/rho/skills/rho-wayland
                cp -r ${./.agents/skills/rho-workstreams} $out/share/rho/skills/rho-workstreams
                chmod -R u+w $out/share/rho/skills
              '';
            };

            tests = craneLib.cargoNextest {
              cargoArtifacts = workspace;
              cargoNextestExtraArgs = "--workspace --show-progress none";
              nativeBuildInputs = [ pkgs.ripgrep ];
            };

            clippy = craneLib.cargoClippy {
              cargoArtifacts = workspaceDeps;
              cargoClippyExtraArgs = "-- -D warnings";
            };

            workspaceDepsCcov = craneLib.buildDepsOnly {
              pname = "${projectName}-workspace-ccov";
              buildPhaseCargoCommand = ''
                source <(cargo llvm-cov show-env --export-prefix)
                cargo build --locked --workspace --all-targets --profile $CARGO_PROFILE
              '';
              cargoBuildCommand = "dontuse";
              cargoCheckCommand = "dontuse";
              nativeBuildInputs = [ pkgs.cargo-llvm-cov ];
              doCheck = false;
            };

            workspaceCcov = craneLib.buildWorkspace {
              pname = "${projectName}-workspace-ccov";
              cargoArtifacts = workspaceDepsCcov;
              buildPhaseCargoCommand = ''
                source <(cargo llvm-cov show-env --export-prefix)
                cargo build --locked --workspace --all-targets --profile $CARGO_PROFILE
              '';
              nativeBuildInputs = [ pkgs.cargo-llvm-cov ];
              doCheck = false;
            };

            testsCcov = craneLib.mkCargoDerivation {
              pname = "${projectName}-tests-ccov";
              cargoArtifacts = workspaceCcov;
              buildPhaseCargoCommand = ''
                source <(cargo llvm-cov show-env --export-prefix)
                cargo nextest run --locked --workspace --all-targets --cargo-profile $CARGO_PROFILE --show-progress none
                mkdir -p $out
                cargo llvm-cov report --profile $CARGO_PROFILE --lcov --output-path $out/lcov.info
                test -s $out/lcov.info
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [
                pkgs.cargo-llvm-cov
                pkgs.cargo-nextest
                pkgs.ripgrep
              ];
              doCheck = false;
            };

          }
        );
      in
      {
        packages = {
          default = multiBuild.package;
          rho = multiBuild.package;
          workspace = multiBuild.workspace;
          inherit webui;
        };

        ci = {
          inherit (multiBuild)
            workspace
            clippy
            tests
            workspaceCcov
            testsCcov
            ;
        };

        legacyPackages = multiBuild;

        devShells = flakeboxLib.mkShells {
          channel = "latest";
          components = flakeboxLib.config.toolchain.components ++ [
            "rustc-codegen-cranelift-preview"
          ];
          NEXTEST_SHOW_PROGRESS = "none";
          RHO_LOG = "rho_agent=debug,info";
          RHO_WAYLAND_SWAY = "${pkgs.sway}/bin/sway";
          RHO_WAYLAND_SWAYMSG = "${pkgs.sway}/bin/swaymsg";
          RHO_WAYLAND_GRIM = "${pkgs.grim}/bin/grim";
          RHO_WAYLAND_WTYPE = "${pkgs.wtype}/bin/wtype";
          RHO_WAYLAND_VK_DRIVER_FILES = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.parsed.cpu.name}.json";
          packages = [
            selfciMq
            pkgs.cargo-nextest
            pkgs.clang
            pkgs.cmake
            pkgs.pkg-config
            pkgs.protobuf
            pkgs.taplo
            selfciPkg
          ]
          ++ guiBuildInputs;
          PROTOC = "${pkgs.protobuf}/bin/protoc";
          LD_LIBRARY_PATH = guiLibraryPath;
          NIX_LD_LIBRARY_PATH = guiLibraryPath;
          shellHook = ''
            ${public-skills.packages.${system}.install}/bin/install-maan2003-skills
            # Flakebox sets target-specific RUSTFLAGS (wild linker), which
            # shadow build.rustflags from .cargo/config.toml; re-add the
            # tokio_unstable cfg that dial9-tokio-telemetry needs.
            export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="''${CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS:-} --cfg tokio_unstable"
            export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS="''${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS:-} --cfg tokio_unstable"
          '';
        };
      }
    );
}
