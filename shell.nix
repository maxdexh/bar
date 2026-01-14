{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  nativeBuildInputs = [pkgs.pkg-config];

  buildInputs = [
    pkgs.rustPlatform.bindgenHook
    pkgs.libpulseaudio
    pkgs.pulseaudio
    pkgs.wlr-randr
  ];
}
