TARGET_DIR := $(shell ./get-target-dir)/wasm32-unknown-unknown/release

.PHONY : world
.DELETE_ON_ERROR :

# The default target.
world : packed.wasm

# Compile the BIOS into a .wasm file.
$(TARGET_DIR)/oc-wasm-bios.wasm : src/main.rs
	cargo build --release

# Strip debug symbols to save some space.
build/stripped.wasm : $(TARGET_DIR)/oc-wasm-bios.wasm
	mkdir -p build
	cp $< $@
	wasm-strip $@

# LZ4-compress the stripped file to save even more space.
build/stripped.wasm.lz4 : build/stripped.wasm
	lz4 -12 -c $< > $@

# Compile the decompressor.
build/decompressor.wasm : decompressor.wat
	mkdir -p build
	wat2wasm -o $@ $<

# Pack the decompressor and the compressed file together to form the final
# output.
packed.wasm : build/decompressor.wasm build/stripped.wasm.lz4 pack
	./pack -o $@ build/decompressor.wasm build/stripped.wasm.lz4

-include $(TARGET_DIR)/oc_wasm_bios.d
