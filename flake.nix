{
  description = "LDU release binaries for Linux and macOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      version = "0.1.0";
      artifacts = {
        x86_64-linux = {
          name = "ldu-linux-x86_64";
          hash = "sha256-SPll1Hypy46dF2jhKralBrdkzp/ai8a2wPLQlYXwELQ=";
        };
        aarch64-linux = {
          name = "ldu-linux-arm64";
          hash = "sha256-ZCIgyvyQJy/0h3gp3Bq7kyBiC8/Jg4I1eevUSRKA8dc=";
        };
        x86_64-darwin = {
          name = "ldu-macos-x86_64";
          hash = "sha256-nMtFAtS+9jaCSNt+YYW7qc0zkPlWjZRWStKcHvhrJgE=";
        };
        aarch64-darwin = {
          name = "ldu-macos-arm64";
          hash = "sha256-fn1msbeRPY5MVZdtx3dVDJlVHAjb32JeG9gf1mDGuB0=";
        };
      };
      forSystems = nixpkgs.lib.genAttrs (builtins.attrNames artifacts);
    in
    {
      packages = forSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          artifact = artifacts.${system};
          ldu = pkgs.stdenvNoCC.mkDerivation {
            pname = "ldu";
            inherit version;
            src = pkgs.fetchurl {
              url = "https://github.com/SmallThingz/ldu/releases/download/v${version}/${artifact.name}";
              inherit (artifact) hash;
            };

            dontUnpack = true;
            dontConfigure = true;
            dontBuild = true;
            # Retain embedded notices and the macOS release signatures byte for byte.
            dontFixup = true;
            installPhase = ''
              runHook preInstall
              mkdir -p "$out/bin"
              install -m755 "$src" "$out/bin/ldu"
              runHook postInstall
            '';
            doInstallCheck = true;
            installCheckPhase = ''
              runHook preInstallCheck
              cmp "$src" "$out/bin/ldu"
              "$out/bin/ldu" --version
              "$out/bin/ldu" --help > /dev/null
              runHook postInstallCheck
            '';

            meta = {
              description = "Live disk usage with effort-first scanning";
              homepage = "https://github.com/SmallThingz/ldu";
              license = nixpkgs.lib.licenses.mit;
              sourceProvenance = [ nixpkgs.lib.sourceTypes.binaryNativeCode ];
              platforms = builtins.attrNames artifacts;
              mainProgram = "ldu";
            };
          };
        in
        {
          default = ldu;
          inherit ldu;
        });

      apps = forSystems (system:
        let
          ldu = {
            type = "app";
            program = "${self.packages.${system}.ldu}/bin/ldu";
          };
        in
        {
          default = ldu;
          inherit ldu;
        });
    };
}
