{
  description = "pijul, the sound distributed version control system";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay, ... }:
    (flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Build pijul without git feature
        pijul = cargoNix.workspaceMembers.pijul.build;

        # Import the generated Cargo.nix
        cargoNix = pkgs.callPackage ./Cargo.nix {
          # Additional build inputs for all crates
          defaultCrateOverrides = pkgs.defaultCrateOverrides // {
            pijul = attrs: {
              nativeBuildInputs = with pkgs; [
                pkg-config
              ];
              buildInputs = with pkgs; [
                openssl
                libsodium
                zstd
                dbus
              ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
                pkgs.darwin.apple_sdk.frameworks.SystemConfiguration
                pkgs.libiconv
              ];
            };
            pijul-core = attrs: {
              nativeBuildInputs = with pkgs; [
                pkg-config
              ];
              buildInputs = with pkgs; [
                openssl
                libsodium
                zstd
              ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
                pkgs.libiconv
              ];
              meta = {
                description = "A distributed version control system";
                homepage = "https://pijul.org";
                license = pkgs.lib.licenses.gpl2Plus;
              };
            };
            libsodium-sys = attrs: {
              nativeBuildInputs = with pkgs; [
                pkg-config
              ];
              buildInputs = with pkgs; [
                libsodium
              ];
            };
          };
        };


        # Build pijul with git feature
        pijul-git = cargoNix.workspaceMembers.pijul.build.override {
          features = [ "git" ];
        };

        # VSCode + Claude setup. Reuses the patched `pijul` built
        # above, so the extension get `record --from-change` natively.
        # Needs allowUnfree (bundles vscode + the Claude Code
        # extension).
        vscodeSetup = import ./editors/vscode/pijul-code.nix {
          pkgs = import nixpkgs { inherit system; config.allowUnfree = true; };
          inherit pijul;
        };

        # Emacs with the Pijul modes, binary paths baked in (fixes GUI-Emacs
        # PATH issues). Reuses the patched pijul.
        emacsSetup = import ./editors/emacs/pijul-emacs.nix {
          inherit pkgs;
          inherit pijul;
        };
      in
      {
        packages = {
          default = pijul;
          inherit pijul pijul-git;
          inherit (vscodeSetup) code;
          # `nix run .#emacs` — Emacs with the Pijul modes + baked binaries.
          # `pijul-emacs` is the bare elisp package to add to your own
          # emacsWithPackages list (see editors/emacs/pijul-emacs.nix).
          inherit (emacsSetup) emacs pijul-emacs;
        };
        devShells.default = pkgs.mkShell {
          inputsFrom = [ pijul ];
          packages = with pkgs; [
            rust-analyzer
            rustfmt
            clippy
            cargo-audit
          ];
        };
        # `nix develop .#vscode --command code` — VSCode with rust-analyzer,
        # our pijul-claude extension, Claude Code, and the patched pijul.
        devShells.vscode = vscodeSetup.shell;
      }
    )) // {
      # Nixpkgs overlay: adds `pkgs.pijul-emacs` (the elisp package,
      # built against the consuming pkgs' Emacs, with the flake's
      # patched pijul baked in). In your configuration.nix:
      # nixpkgs.overlays = [ inputs.pijul.overlays.default ]; # then,
      # in your emacsWithPackages list: pkgs.pijul-emacs
      overlays.default = final: _prev:
        let
          es = import ./editors/emacs/pijul-emacs.nix {
            pkgs = final;
            pijul = self.packages.${final.stdenv.hostPlatform.system}.pijul;
          };
        in
        { inherit (es) pijul-emacs; };
    };
}
