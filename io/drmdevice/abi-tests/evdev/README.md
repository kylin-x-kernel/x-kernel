# evdev regression cases

A standalone userspace Cargo package that checks the ioctl dispatch layer of evdev: the probes libinput makes when it opens an input
device (`EVIOCGVERSION`, `EVIOCGID`, `EVIOCGNAME`, `EVIOCGPHYS`,
`EVIOCGPROP`, `EVIOCGKEY`, `EVIOCGBIT`, `EVIOCGABS`) must all be answered with the
length encoded in the request, not with an arbitrary length.

- The command words are all hard-coded to the Linux values, so layout drift makes the call fall to `EINVAL` instead of quietly passing;
- The name must be non-empty (libinput identifies a device by its name);
- The length of the property/key bitmap answers equals the request length (zero-padded);
- An unknown axis number returns `EINVAL`;
- A device without `EV_ABS` returns `EINVAL` for `EVIOCGABS`, while an absolute-axis device returns the real range.

Build and run:

```sh
cd io/drmdevice/abi-tests/evdev
cargo test --target aarch64-unknown-linux-musl --no-run
```

Copy the generated executable into the X-Kernel user environment and run it:

```sh
EVENT_DEVICE=/dev/input/event0 /usr/tests/evdev_regression --nocapture
```
