# Shared IRQ value safety

Only Copy values and integer wrappers exist, with no unsafe, I/O or global state.
The host validates domain existence, routes and trigger support. A domain number
does not retain its entry or prevent reuse. Extraction preserves variants,
numeric representation and comparisons, not host registration authority.
