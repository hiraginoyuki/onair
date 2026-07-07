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
      version = "0.2.0";
      tag = "unstable";
      hashes = {
        "x86_64-unknown-linux-gnu" = "sha256-LwkRRywJDS1gs1wZyBF8idCfgILjaHSeFZljbvzU1c8=";
        "x86_64-unknown-linux-musl" = "sha256-kDva5ZoM/HAt3MMvYpDC5DBerKMnJEPdYy4Z8m3FI9o=";
        "aarch64-unknown-linux-gnu" = "sha256-ts5iR4DFZSp8J0Taws4y0zGp80nS2IjmUcvz/u/pWUk=";
        "aarch64-unknown-linux-musl" = "sha256-jIF0XQXwSptbh/WV1VAFEuuN+f0SjxO4ZRPesAOHLwE=";
        "x86_64-apple-darwin" = "sha256-HreNje5rA32a1ZqF+zfN2B91qs8eBRHDK0KPJP6VK6I=";
        "aarch64-apple-darwin" = "sha256-KgabegSw4fQfjjTSBL+gH7M77lKAff6CX9HtUESsdi4=";
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
