```nix
{
  inputs.v-utils.url = "github:valeratrades/.github";

  outputs = { self, nixpkgs, v-utils, ... }:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
      pname = "my-project";

      rs = v-utils.rs {
        inherit pkgs;
        tracey = true;
        style.format = true;
        style.modules = {
          prefer_ahash = true; # enforce ahash over std HashMap (default: false)
        };
      };

      github = v-utils.github {
        inherit pkgs pname rs;
        langs = [ "rs" ];
        jobs.default = true;
      };

      readme = v-utils.readme-fw {
        inherit pkgs pname;
        rootDir = ./.;
        lastSupportedVersion = "nightly-1.86";
        defaults = true;
        badges = [ "msrv" "crates_io" "docs_rs" "loc" "ci" ];
      };

      # Combine all modules - automatically collects enabledPackages and shellHook
      combined = v-utils.utils.combineModules [ rs github readme ];
    in
    {
      devShells.default = pkgs.mkShell {
        inherit (combined) shellHook;
        packages = combined.enabledPackages;
      };
    };
}
```
