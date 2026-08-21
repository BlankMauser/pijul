with import <nixpkgs> {
  overlays = map (uri: import (fetchTarball uri)) [
    https://github.com/oxalica/rust-overlay/archive/master.tar.gz
  ];
};

stdenv.mkDerivation {
  name = "Pijul";
  buildInputs = with pkgs; [
    zstd
    libsodium
    openssl
    pkg-config
    libiconv
    xxHash
    dbus
    (rust-bin.stable.latest.default.override {
      targets = [
        "x86_64-unknown-linux-gnu"
        "x86_64-pc-windows-msvc"
      ];
    })
  ] ++ lib.optionals stdenv.isDarwin
    (with darwin.apple_sdk.frameworks; [
      CoreServices
      Security
      SystemConfiguration
    ]);
}
