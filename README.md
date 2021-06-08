Overview
========

OC-Wasm BIOS is a basic BIOS for computers running the
[OC-Wasm](https://gitlab.com/Hawk777/oc-wasm) architecture. Unlike the Lua
BIOS, it does not provide any runtime services; its purpose is solely to find
and boot a bootable medium. It does this using logic almost identical to the
Lua BIOS. First, it checks whether the computer’s EEPROM contains a data
string; if so, and if that data string is the address of a filesystem
component (in the form of a binary UUID), and if that filesystem contains a
file named `init.wasm` in its root directory, then that file is loaded and
executed. Otherwise, it scans all reachable filesystem components (typically
hard and floppy disks, though anything of `filesystem` type is considered);
when it finds one whose root directory contains a file named `init.wasm`, that
file is loaded and executed.

No validation is performed on the located `init.wasm` file before execution. If
a file named `init.wasm` appears in the root directory of a filesystem but is
not a valid WebAssembly binary or for any other reason cannot be executed, the
computer will crash.

Because there isn’t a hand-craftable item available preloaded with OC-Wasm
BIOS, in order to obtain an EEPROM with the OC-Wasm BIOS, one must first boot a
computer using a different architecture, copy the BIOS image into that computer
as a file (e.g. by copying the file into the Minecraft save directory or by
downloading the file using an Internet card), and write the file to a writeable
EEPROM. For example, if using OpenOS under the Lua architecture, one could use
the `flash` program.

The binary image can be obtained by downloading a precompiled image from a
release or by compiling the BIOS yourself.


Compiling
=========

To compile the BIOS yourself, you will need the following software:

* [Rust](https://rust-lang.org/), with the `wasm32-unknown-unknown` target
  enabled
* [GNU Make](https://gnu.org/software/make), or another similar Make
* [LZ4](https://lz4.github.io/lz4/) (the command-line `lz4` tool)
* [WebAssembly Binary Toolkit](https://github.com/WebAssembly/wabt)
* [Python](https://python.org/) 3

A typical Linux distribution will have packages for some of these, but
depending on your distribution, you may need to build some of them from sources
or look for third-party packages. Once you have the prerequisites installed,
simply run `make` to compile the BIOS. The output will be placed in the file
`packed.wasm`. This file must be small enough to fit on an EEPROM, which means
≤4096 bytes; the size may vary a little depending on which versions of various
software (especially Rust and its associated LLVM) you are using, but as of
this writing, in my environment, with Rust 1.52.1, it is 2976 bytes.


Architecture
============

The BIOS is implemented in two stages. The main logic of the BIOS—searching for
bootable media and loading and executing `init.wasm` from it—is written in Rust
and forms the second stage. While it is quite compact, it is still
significantly larger than the required 4096 bytes, even after debug information
is stripped. Therefore, it is compressed using the LZ4 compression algorithm at
its best setting, chosen because it is extremely simple to decompress.

Because LZ4-compressed binaries cannot be executed directly, a first stage is
needed. The first stage is the file `decompressor.wat`; it is a hand-written
WebAssembly module that decompresses the LZ4-compressed second stage and then
executes it. For simplicity, `decompressor.wat` is limited to decompressing
small files; however, these limits are encoded in the linear memory size and
global variables, so it could be repurposed as an unpacker for general
compressed executables quite easily.

The two stages are combined by the `pack` Python script. This reads the
compiled form of the first stage, `decompressor.wasm`, and injects the
LZ4-compressed second stage as a segment in its data section. It also updates a
global to encode the size of the input. Finally, it writes out the output to a
new Wasm module, `packed.wasm`.
