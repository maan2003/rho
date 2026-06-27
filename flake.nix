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

        multiBuild = (flakeboxLib.craneMultiBuild { toolchains = muslToolchains; }) (
          craneLib':
          let
            craneLib = craneLib'.overrideArgs {
              pname = projectName;
              src = buildSrc;
              nativeBuildInputs = [ ];
              env.RUSTDOCFLAGS = "-D warnings";
              CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "";
              CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "";
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
            pkgs.taplo
            selfciPkg
          ];
          shellHook = ''
            ${dpc-public-skills.packages.${system}.install}/bin/install-dpc-public-skills
          '';
        };
      }
    );
}
