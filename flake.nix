{
	inputs = {
		nixpkgs.url = "github:nixos/nixpkgs/release-25.05";
	};

	# Rendering is entirely on the CPU now (vello_cpu) and video is muxed in
	# process (less-avc + mp4), so the Vulkan loader and the GStreamer stack the
	# old wgpu pipeline needed are gone — along with the RUSTFLAGS rpath that
	# only existed to find libvulkan at runtime.
	outputs = { self, nixpkgs }:
		let pkgs = nixpkgs.legacyPackages.x86_64-linux;
		in {
			devShells.x86_64-linux.default = pkgs.mkShell {
				packages = with pkgs; [
					cargo
					clippy
					rustfmt
					pkg-config
					just
				];
			};
		};
}
