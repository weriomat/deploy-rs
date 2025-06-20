# SPDX-FileCopyrightText: 2020 Serokell <https://serokell.io/>
#
# SPDX-License-Identifier: MPL-2.0

{
  description = "Deploy a full system with hello service as a separate profile";

  inputs.deploy-rs.url = "github:serokell/deploy-rs";

  outputs =
    {
      self,
      nixpkgs,
      deploy-rs,
    }:
    {
      nixosConfigurations.example-nixos-system = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./configuration.nix
          "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
        ];
      };

      nixosConfigurations.bare = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          ./bare.nix
          "${nixpkgs}/nixos/modules/virtualisation/qemu-vm.nix"
        ];
      };

      # This is the application we actually want to run
      packages.x86_64-linux.default = import ./hello.nix nixpkgs;

      deploy.nodes.example = {
        sshOpts = [
          "-p"
          "2221"
        ];
        hostname = "localhost";
        fastConnection = true;
        profiles = {
          system = {
            sshUser = "admin";
            path = deploy-rs.lib.x86_64-linux.activate.nixos self.nixosConfigurations.example-nixos-system;
            user = "root";
          };
          hello = {
            sshUser = "hello";
            path = deploy-rs.lib.x86_64-linux.activate.custom self.packages.x86_64-linux.default "./bin/activate";
            user = "hello";
          };
        };
      };

      checks = builtins.mapAttrs (system: deployLib: deployLib.deployChecks self.deploy) deploy-rs.lib;
    };
}
