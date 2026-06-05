{
  description = "onair — OpenAI-compatible reverse proxy router";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    onair-meta = {
      url = "github:hiraginoyuki/onair/nix-metadata";
      flake = false;
    };
  };

  outputs =
    { self
    , nixpkgs
    , onair-meta
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
      meta = builtins.fromJSON (builtins.readFile "${onair-meta}/metadata.json");
      selected = meta.${meta.default};
      mkPkg = system: pkgs: pkgs.stdenv.mkDerivation {
        pname = "onair";
        version = selected.version or meta.default;
        src = pkgs.fetchurl {
          url = "https://github.com/hiraginoyuki/onair/releases/download/${selected.tag}/onair-${targetOf system}";
          sha256 = selected.sha256.${targetOf system};
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
