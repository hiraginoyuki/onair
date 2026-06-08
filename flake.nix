{
  description = "onair — OpenAI-compatible reverse proxy router";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self
    , nixpkgs
    ,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = nixpkgs.legacyPackages;
      targetOf = system: {
        x86_64-linux   = "x86_64-unknown-linux-gnu";
        aarch64-linux  = "aarch64-unknown-linux-gnu";
        x86_64-darwin  = "x86_64-apple-darwin";
        aarch64-darwin = "aarch64-apple-darwin";
      }.${system};

      # Release metadata — updated by the release bot on every release.
      # To pin a specific release, point a Nix ref at the commit that
      # contains the values you want (e.g. `?ref=v0.1.0`).
      version = "0.1.0";
      tag = "unstable";
      hashes = {
        "x86_64-unknown-linux-gnu" = "sha256-KcinxSHD5mol+21LGGTXmePWsdIsXMwNaRTpjkQnK88=";
        "x86_64-unknown-linux-musl" = "sha256-TT0qZnW0gMcCnqxEH4b46gznTz/NSFfswV6eNoa6MHE=";
        "aarch64-unknown-linux-gnu" = "sha256-zsYOg6d8+bflt6uL+BeaFK2nj6pkoDU/nqa5jhukfuk=";
        "aarch64-unknown-linux-musl" = "sha256-lG3hqqDjFSJZyGJmPDmsfRe0/u9OJEH1njj5+eYzZog=";
        "x86_64-apple-darwin" = "sha256-WYT9ccMHtXh1IyRUmzPhKofwTNsgiok5Ns9RG3DGWB0=";
        "aarch64-apple-darwin" = "sha256-hmp9QmG9j8hYAkzWhAr/YmCldRDJg6g4CjAvsO/6l3U=";
      };

      mkPkg = system: pkgs: pkgs.stdenv.mkDerivation {
        pname = "onair";
        inherit version;
        src = pkgs.fetchurl {
          url = "https://github.com/hiraginoyuki/onair/releases/download/${tag}/onair-${targetOf system}";
          sha256 = hashes.${targetOf system} or (throw "onair flake: no sha256 hash configured for target ${targetOf system} (system ${system}) — add it to the `hashes` attrset in flake.nix");
        };
        dontUnpack = true;
        dontConfigure = true;
        dontBuild = true;
        dontStrip = true;
        installPhase = ''
          runHook preInstall
          install -Dm755 $src $out/bin/onair
          runHook postInstall
        '';
        meta = {
          description = "OpenAI-compatible reverse proxy router";
          homepage = "https://github.com/hiraginoyuki/onair";
          license = pkgs.lib.licenses.mit;
          mainProgram = "onair";
          platforms = [
            "x86_64-linux"
            "aarch64-linux"
            "x86_64-darwin"
            "aarch64-darwin"
          ];
        };
      };
    in
    {
      packages = forAllSystems (system: {
        default = mkPkg system pkgsFor.${system};
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/onair";
        };
      });
    };
}
