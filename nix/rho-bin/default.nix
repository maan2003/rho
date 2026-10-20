# `rho` from the archive `build.nix` publishes: the binaries built
# elsewhere, patched for this machine, with the runtime they find at
# ../share/rho beside them (`rho_fs_view::bundled`) built here from the
# same pins as the `rho` package. Nothing comes from a cache but
# cache.nixos.org.
{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  zstd,
  python3,
  agentBase,
  pythonSitePackages,
  findutils,
  cargoSharedCache,
}:

let
  # Written by `rho-bin-build release`, one archive a system.
  pin = (lib.importJSON ./pin.json).${stdenv.hostPlatform.system};
in
stdenv.mkDerivation {
  pname = "rho-bin";
  inherit (pin) version;

  src = fetchurl { inherit (pin) url hash; };
  sourceRoot = ".";

  nativeBuildInputs = [
    autoPatchelfHook
    zstd
  ];
  # libpython is the one the notebook was linked against: the same pin.
  buildInputs = [
    (lib.getLib stdenv.cc.cc)
    python3
  ];

  installPhase = ''
    runHook preInstall
    mkdir -p $out/share/rho
    cp -r bin $out/
    cp -r share/rho/. $out/share/rho/
    ln -s ${agentBase} $out/share/rho/agent-base
    ln -s ${pythonSitePackages} $out/share/rho/python-site-packages
    ln -s ${findutils}/bin $out/share/rho/find-bin
    ln -s ${cargoSharedCache}/bin $out/share/rho/shared-cargo-bin
    runHook postInstall
  '';

  meta = {
    description = "Rho's CLI and agent host, prebuilt";
    mainProgram = "rho";
    platforms = builtins.attrNames (lib.importJSON ./pin.json);
  };
}
