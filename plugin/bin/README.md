# Legacy binary target note

Hermes does not build binaries through `plugin.yaml` lifecycle hooks. Task 0.2 implements the explicit `hermes caduceus setup` command, which builds with Cargo's locked dependency graph and atomically installs the daemon under the root plugin's generated `bin/` directory.
