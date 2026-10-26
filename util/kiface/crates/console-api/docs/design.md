# kiface-console-api Design

The console driver supplies three independent contracts. Ordinary byte I/O may use its locks. EmergencyConsoleWriteIf uses bounded best-effort output. ConsoleInputSetupIf owns one-time optional input activation and installed-line discovery. The HAL keeps convenient functions but owns no serial buffers or setup state.

See [capability ownership rules](../../../docs/design.md) for the common
contract/provider boundary. This crate contains no implementation state.
