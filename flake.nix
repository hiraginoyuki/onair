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
        "x86_64-unknown-linux-gnu" = "sha256-RShDEnfTek6uA8D1tXY26mPXIwnitusqv6L96QsQX6o=";
        "x86_64-unknown-linux-musl" = "sha256-R3ndJTIAc5cdZrddt7pTVjoMwPpwv01/BCjdVlJmj4w=";
        "aarch64-unknown-linux-gnu" = "sha256-MabvXrBVgfUC8Ot0E24gZUC3OfOgz8TU1f4biH8YaCI=";
        "aarch64-unknown-linux-musl" = "sha256-dpgY0F4wCY/cs1UOZv6kp54GAv2J/7VyVc+9Utho08g=";
        "x86_64-apple-darwin" = "sha256-X7DNnLRgOA+wZJ8aJAC18/iM8C2JkR4FKtE574nRqjw=";
        "aarch64-apple-darwin" = "sha256-e8lrFgeyfo1oYYHsxswnV1Dof80oFpQwPak0R4O66dI=";
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
