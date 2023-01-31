(module
	;; Function types.
	(type $void_type (func))
	(type $void_i32_i32_type (func (param i32) (param i32)))
	(type $i32_type (func (result i32)))
	(type $i32_i32_type (func (param i32) (result i32)))
	(type $i32_i32_i32_type (func (param i32) (param i32) (result i32)))

	;; Imports.
	(import "execute" "add" (func $execute_add (type $i32_i32_i32_type)))
	(import "execute" "execute" (func $execute_execute (type $void_type)))

	;; The base address of the output region where the decompressed data will
	;; be stored.
	(global $write_base i32 (i32.const 4096))
	;; The next address to read LZ4-compressed data from.
	(global $read_ptr (mut i32) (i32.const 0))
	;; The length of the LZ4-compressed data which, since it starts at address
	;; zero, is also the limit pointer. The pack script will replace the
	;; initialization value with the actual value.
	(global $read_limit i32 (i32.const 0x55AA55AA))
	;; The next address to write decompressed data to.
	(global $write_ptr (mut i32) (i32.const 4096))

	;; The application entry point.
	(func $run
		(type $i32_i32_type)
		(param $indirect_call_completed i32)
		(result i32)
		(local $scratch i32)
		(local $match_length i32)
		;; Loop over the sequences.
		(loop $sequences_loop
			;; Read the token and split it into the upper four bits (literal
			;; length) and the lower four bits (match length, without adding
			;; four yet). Store the match length in $match_length but leave the
			;; literal length on the stack.
			(call $read_8)
			(local.tee $scratch)
			(local.get $scratch)
			(i32.const 0xF)
			(i32.and)
			(local.set $match_length)
			(i32.const 4)
			(i32.shr_u)
			;; Extend the literal length, if needed, leaving a copy in
			;; $scratch.
			(call $extend_length)
			(local.tee $scratch)
			;; Copy the literal data.
			(global.get $read_ptr)
			(call $write_bytes)
			;; Advance the read pointer using the literal length saved in
			;; $scratch.
			(global.get $read_ptr)
			(local.get $scratch)
			(i32.add)
			(global.set $read_ptr)
			;; A block ends after literals without any match. Check for that
			;; case.
			(global.get $read_ptr)
			(global.get $read_limit)
			(i32.ne)
			(if (then
				;; Read the match offset and calculate the source pointer,
				;; leaving it in $scratch.
				(global.get $write_ptr)
				(global.get $read_ptr)
				(i32.load16_u)
				(i32.sub)
				(local.set $scratch)
				;; Advance $read_ptr over the two read bytes.
				(global.get $read_ptr)
				(i32.const 2)
				(i32.add)
				(global.set $read_ptr)
				;; Extend the match length, if needed.
				(local.get $match_length)
				(call $extend_length)
				;; Add four to the length because that’s how matches roll.
				(i32.const 4)
				(i32.add)
				;; Copy the match data.
				(local.get $scratch)
				(call $write_bytes)
				;; Go back and do the next sequence.
				(br $sequences_loop))))
		;; Execute the decompressed code. It starts at address $write_base, and
		;; its length is ($write_ptr−$write_base).
		(global.get $write_base)
		(global.get $write_ptr)
		(global.get $write_base)
		(i32.sub)
		(call $execute_add)
		(call $execute_execute)
		;; We don’t need an (i32.const 0) here for the return value because
		;; $execute_add returns a value which we have so far ignored. We’ll
		;; never actually get here because $execute_execute never returns, so
		;; the return value is irrelevant, so we can save space by avoiding a
		;; (drop) plus (i32.const 0).
		)

	;; Given the initial nybble of a length as a parameter, reads zero or more
	;; additional bytes from $read_ptr to extend it, then returns the total length.
	(func $extend_length
		(type $i32_i32_type)
		(param $length i32)
		(result i32)
		(local $scratch i32)
		(local.get $length)
		(i32.const 15)
		(i32.eq)
		(if (then
			(loop $extra_bytes
				(call $read_8)
				(local.tee $scratch)
				(local.get $length)
				(i32.add)
				(local.set $length)
				(local.get $scratch)
				(i32.const 255)
				(i32.eq)
				(br_if $extra_bytes))))
		(local.get $length))

	;; Reads one byte from $read_ptr.
	(func $read_8
		(type $i32_type)
		(result i32)
		(global.get $read_ptr)
		(i32.load8_u)
		(global.get $read_ptr)
		(i32.const 1)
		(i32.add)
		(global.set $read_ptr))

	;; Copies $count bytes from $src to $write_ptr, advancing $write_ptr.
	(func $write_bytes
		(type $void_i32_i32_type)
		(param $count i32)
		(param $src i32)
		;; Loop over the bytes.
		(loop $lp
			;; Check for zero count.
			(local.get $count)
			i32.eqz
			(if (then) (else
				;; Copy the byte from $src to $write_ptr
				(global.get $write_ptr)
				(local.get $src)
				(i32.load8_u)
				(i32.store8)
				;; Increment $write_ptr.
				(global.get $write_ptr)
				(i32.const 1)
				(i32.add)
				(global.set $write_ptr)
				;; Increment $src.
				(local.get $src)
				(i32.const 1)
				(i32.add)
				(local.set $src)
				;; Decrement $count.
				(local.get $count)
				(i32.const 1)
				(i32.sub)
				(local.set $count)
				;; Return to the top of the loop.
				(br $lp)))))

	;; 64 kiB is plenty; that gives 4 kiB for the LZ4-compressed input (which
	;; is more than it could possibly be, since that *plus this decompressor*
	;; must be ≤4 kiB to fit on an EEPROM) and 60 kiB for the decompressed
	;; output.
	(memory 1)

	;; Export the run function so it can be called by the host.
	(export "run" (func $run)))
