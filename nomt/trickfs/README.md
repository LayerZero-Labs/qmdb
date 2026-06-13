# trickfs

A FUSE filesystem useful for failure injection.

# Using trickfs.

Typically you would not need to run trickfs directly, because it should be used as a dependency
in other projects. However, if you want to test the filesystem, you can do so by running the
following command:

```sh
cargo run --release --bin trickmnt
```

# Building

Building the project requires fuse3 and fuse to be available. On Ubuntu, you can install them with
the following commands:

```sh
sudo apt update
sudo apt install libfuse3-dev libfuse-dev
```

On macOS you may need to install osxfuse:

```sh
brew install macfuse
```
