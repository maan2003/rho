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
        cargoCrap = pkgs.callPackage ./nix/pkgs/cargo-crap.nix { };
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
                    CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS = "";
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
        ];

        buildSrc = flakeboxLib.filterSubPaths {
          root = builtins.path {
            name = projectName;
            path = ./.;
          };
          paths = buildPaths;
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
        zedSrc = pkgs.fetchFromGitHub {
          owner = "maan2003";
          repo = "zed";
          rev = "f08a95d674b49474912b8c03ef51917cb042c606";
          hash = "sha256-0rgm2Wh26AQo0iAPo+KJTuj66Jf8SbLEig5Cwnm4kH4=";
        };
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
              env.RUSTY_V8_ARCHIVE = rustyV8Archive;
              CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "";
              CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "";
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
            workspaceDeps = craneLib.buildWorkspaceDepsOnly { };

            workspace = craneLib.buildWorkspace {
              cargoArtifacts = workspaceDeps;
            };

            package = craneLib.buildPackage {
              cargoArtifacts = workspaceDeps;
              cargoExtraArgs = "-p rho-cli -p rho-daemon -p git-remote-octo";
              doCheck = false;
              env.RHO_BUNDLED_SKILLS_DIR = "${builtins.placeholder "out"}/share/rho/skills";
              postInstall = ''
                mkdir -p $out/share/rho/skills
                cp -r ${./.agents/skills/github-workflow} $out/share/rho/skills/github-workflow
                cp -r ${./.agents/skills/delegate-engineering} \
                  $out/share/rho/skills/delegate-engineering
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

            crapReport = craneLib.mkCargoDerivation {
              pname = "${projectName}-cargo-crap-ccov-report";
              cargoArtifacts = workspaceCcov;
              buildPhaseCargoCommand = ''
                test -s ${testsCcov}/lcov.info
                mkdir -p $out
                ${cargoCrap}/bin/cargo-crap \
                  --workspace \
                  --lcov ${testsCcov}/lcov.info \
                  --top 100 \
                  --min 50 \
                  --format markdown \
                  --output $out/cargo-crap.md
                cp ${testsCcov}/lcov.info $out/lcov.info
              '';
              doInstallCargoArtifacts = false;
              nativeBuildInputs = [ cargoCrap ];
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
          "cargo-crap" = cargoCrap;
        };

        ci = {
          inherit (multiBuild)
            workspace
            clippy
            tests
            workspaceCcov
            testsCcov
            crapReport
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
          packages = [
            cargoCrap
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
          '';
        };
      }
    );
}
