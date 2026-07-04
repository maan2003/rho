{
  description = "rho";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

    flake-utils.url = "github:numtide/flake-utils";
    flakebox = {
      url = "github:rustshop/flakebox?rev=cf89db7a3ac6b1431693d17276225ba352e48a5c";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    dpc-public-skills = {
      url = "git+https://radicle.dpc.pw/z2HR882B4c4mTdAgdt4SozpdeTuMf.git";
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
      dpc-public-skills,
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
          rev = "8c93f816e224648577dcb1c1b58b9445a6633416";
          hash = "sha256-Xw756jnCJPmTjLEPo8KYcplo/9Sk+ccjK9iHD8UhN4Y=";
        };

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
          default = multiBuild.workspace;
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
            ${dpc-public-skills.packages.${system}.install}/bin/install-dpc-public-skills
          '';
        };
      }
    );
}
