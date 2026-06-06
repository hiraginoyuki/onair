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
        "x86_64-unknown-linux-gnu"  = "sha256-5pldhvJ1g9Ku2U1c8XZ2taHKryHY11s2paatjJYxMUc=";
        "x86_64-unknown-linux-musl" = "sha256-yDPS3Faz3pxZOGhZ+hIsXK49VHv7iqsf85jp6U0qsC0=";
        "aarch64-unknown-linux-gnu" = "sha256-AS/wvuL5jn6dWhw4TGZI5whmEq7mFC2oMzt0EGTLe4o=";
        "aarch64-unknown-linux-musl" = "sha256-NUubI+XK9IkD25OzK867EEGcXj3+NPiiK1W69EGLY2E=";
        "x86_64-apple-darwin"       = "sha256-O/6jOqK0r/6sQKT1uGAjyNCqAY9LO5HG1dwmdY6f70M=";
        "aarch64-apple-darwin"      = "sha256-H03PKDFaVu7XVc9Z1cT4bWXq2PfaNLkO6tWGzbYiDt4=";
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
