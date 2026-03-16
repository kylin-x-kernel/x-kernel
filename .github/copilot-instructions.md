## Build Commands
- cp platforms/xxxx/defconfig .config - Copy platforms defconfig to project root
- `make rootfs` - download and prepare root filesystem (copy defconfig before running)
- `make run` - build and run the project (only on qemu platform copy defconfig before running)
- `make build` - build the project for all platforms (copy defconfig before building)
- `make UNITTEST=y run` - run unit tests on qemu platform(copy defconfig before running)
` `make build V=1` - build the project with verbose output (copy defconfig before building)

## Code Style
- Use rust fmt for formatting code

## Workflow
- Before we proceed, please confirm any details you are unclear about with me.
- Run config and build or run commands after making changes
- Commit messages follow conventional commits format
