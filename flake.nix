{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { flake-utils, naersk, nixpkgs, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = (import nixpkgs) { inherit system; };
        naersk' = pkgs.callPackage naersk {};
      in {
        packages.default  = naersk'.buildPackage {
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.openssl ];
          src = pkgs.lib.cleanSourceWith {
            src = pkgs.lib.cleanSource ./.;
            filter = path: type: baseNameOf path != "Settings.toml";
          };
        };
        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ rustc cargo pkg-config ];
          buildInputs = pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.openssl ];
        };
      }
    );
}
