#![no_main]
#![no_std]
#![warn(
	// Turn on extra language lints.
	future_incompatible,
	missing_abi,
	nonstandard_style,
	rust_2018_idioms,
	single_use_lifetimes,
	trivial_casts,
	trivial_numeric_casts,
	unused,
	unused_crate_dependencies,
	unused_import_braces,
	unused_lifetimes,
	unused_qualifications,

	// Turn on extra Clippy lints.
	clippy::cargo,
	clippy::pedantic,
)]
// Don’t use map_or_else; it produces larger code than an if let block.
#![allow(clippy::option_if_let_else)]
// Shadowing is useful when decoding a bunch of CBOR headers one after another.
#![allow(clippy::shadow_unrelated)]
// Uninlining the state machine steps produces larger code.
#![allow(clippy::too_many_lines)]

use core::convert::TryInto;
use core::mem::replace;
use core::panic::PanicInfo;
use core::ptr;
use oc_wasm_safe::{
	component, computer, descriptor, descriptor::AsDescriptor, error, execute, Address,
};
use oc_wasm_sys::component as component_sys;

/// The panic handler used for the BIOS.
#[panic_handler]
fn handle_panic(_: &PanicInfo<'_>) -> ! {
	// Do the absolute bare minimum to stop execution.
	core::arch::wasm32::unreachable();
}

/// Reports an internal error with no more detailed message.
fn internal_error() -> ! {
	computer::error("BIOS: internal error")
}

/// The CBOR major types.
#[derive(Clone, Copy, Eq, PartialEq)]
enum CborMajorType {
	/// The data item is an unsigned integer whose value is equal to the count. There is no
	/// payload.
	UnsignedInteger,

	/// The data item is a negative integer whose value is −1−count. There is no payload.
	NegativeInteger,

	/// The data item is a byte array. The count is the number of bytes, and they are stored in the
	/// payload.
	Bytes,

	/// The data item is a string. The count is the number of bytes in the UTF-8 encoding, and that
	/// encoding is stored in the payload.
	String,

	/// The data item is an array of data items. The count is the number of items in the array, and
	/// they are stored in the payload.
	Array,

	/// The data item is an array of key/value pairs of data items. The count is the number of
	/// pairs in the array, and they are stored in the payload.
	Map,

	/// The data item is a semantic tag. The count is the identity of the tag. The tagged item is
	/// stored in the payload.
	Tag,

	/// The data item is a special value. The count is the value of the data item. There is no
	/// payload.
	Special,

	/// The data item is a floating-point number. The count is the value of the data item. There is
	/// no payload.
	Float,
}

/// Reads a CBOR data item header from a byte slice.
///
/// The `slice` parameter is the byte slice to read from. On success, the major type, raw count
/// value (prior to interpretation according to major type), and a slice containing the rest of the
/// input slice starting immediately following the header (i.e. at the payload, if any, otherwise
/// at the next date item) are returned.
///
/// # Errors
/// * [`BufferTooShort`](error::BufferTooShort) is returned if `slice` is empty.
/// * [`CborDecode`](error::CborDecode) is returned if `slice` is nonempty but does not contain a
///   whole header, or if the header is invalid.
fn cbor_decode_header(slice: &[u8]) -> error::Result<(CborMajorType, u64, &[u8])> {
	// Grab the first byte.
	let first_byte = slice.first().ok_or(error::Error::BufferTooShort)?;
	let slice = &slice[1..];

	// Decode the major type from the upper three bits.
	let major_type = match first_byte >> 5 {
		0 => CborMajorType::UnsignedInteger,
		1 => CborMajorType::NegativeInteger,
		2 => CborMajorType::Bytes,
		3 => CborMajorType::String,
		4 => CborMajorType::Array,
		5 => CborMajorType::Map,
		6 => CborMajorType::Tag,
		7 => match first_byte & 31 {
			25..=27 => CborMajorType::Float,
			_ => CborMajorType::Special,
		},
		_ => unreachable!(), // Impossible; u8>>5 can only be 0..=7.
	};

	// Decode the count.
	let count_bits = first_byte & 31;
	let (count, slice): (u64, &[u8]) = if count_bits <= 23 {
		(count_bits.into(), slice)
	} else {
		let count_bytes = match count_bits {
			24 => 1,
			25 => 2,
			26 => 4,
			27 => 8,
			_ => return Err(error::Error::CborDecode),
		};
		if slice.len() < count_bytes {
			return Err(error::Error::CborDecode);
		}
		let (count_bytes, slice) = slice.split_at(count_bytes);
		let mut count_value: u64 = 0;
		for &byte in count_bytes {
			count_value = (count_value << 8) | Into::<u64>::into(byte);
		}
		(count_value, slice)
	};

	// Return everything.
	Ok((major_type, count, slice))
}

/// When opening a `/init.wasm` file, the two possible ways in which we could have found the UUID
/// of the filesystem component we are accessing.
enum UuidSource {
	/// We read the UUID from the EEPROM, where it identifies the default boot device.
	Eeprom,

	/// We got the UUID from the list of all filesystem components and are scanning for any
	/// bootable medium.
	Scan(component::Listing<'static>),
}

/// The information associated with the [`OpeningFile`](State::OpeningFile) state.
struct OpeningFileInfo {
	/// The UUID of the filesystem component.
	pub uuid: Address,

	/// Where the UUID came from.
	pub source: UuidSource,
}

/// The information associated with the [`ReadingFile`](State::ReadingFile) state.
#[derive(Eq, PartialEq)]
struct ReadingFileInfo {
	/// The file descriptor.
	pub descriptor: descriptor::Owned,

	/// The UUID of the filesystem component.
	pub uuid: Address,
}

/// The state machine that the BIOS moves through while doing its work.
enum State {
	/// The initial state when the BIOS starts running.
	Init,

	/// The EEPROM’s boot device UUID is being read.
	ReadingBootDeviceUuid,

	/// A component listing should be started.
	StartScan,

	/// A component listing is in progress.
	Scanning(component::Listing<'static>),

	/// A method call has been made to open `/init.wasm` on a filesystem.
	OpeningFile(OpeningFileInfo),

	/// A `/init.wasm` file has been opened successfully. We are now reading data from the file and
	/// storing it to the execution buffer.
	ReadingFile(ReadingFileInfo),
}

/// The possible values that a single successful run step can return.
#[derive(Clone, Copy, Eq, PartialEq)]
enum RunResult {
	/// The next step should be taken immediately.
	RunNext,

	/// The [`run`](run) function should return (most likely to allow an indirect method call to
	/// complete).
	Return,
}

/// The filename of the file to open.
const FILENAME: &[u8] = b"/init.wasm";

/// Starts opening `/init.wasm` on a filesystem component.
///
/// The `address` parameter identifies the component by its UUID.
///
/// `true` is returned if the call is complete now. `false` is returned if the call has started but
/// will not be complete until the next timeslice.
fn invoke_open_init(address: &Address) -> bool {
	let mut buffer = [0_u8; 3 + FILENAME.len()];
	// Write the array header.
	buffer[0] = (4 << 5) | 1;
	// Write the filename string.
	buffer[1] = (3 << 5) | 24;
	// Cast is sound because FILENAME is short.
	#[allow(clippy::cast_possible_truncation)]
	{
		buffer[2] = FILENAME.len() as u8;
	}
	// SAFETY: buffer is of length (3 + FILENAME.len()). Therefore buffer[3..] is of length
	// FILENAME.len(). FILENAME.as_ptr() returns *const u8, and u8 impl Copy.
	unsafe {
		ptr::copy_nonoverlapping(FILENAME.as_ptr(), buffer[3..].as_mut_ptr(), FILENAME.len());
	}
	let method = "open";
	let rc = unsafe {
		component_sys::invoke_component_method(
			address.as_bytes().as_ptr(),
			method.as_ptr(),
			method.len(),
			buffer.as_ptr(),
		)
	};
	// If this fails, it indicates a bug in the BIOS, not a problem with the user’s configuration.
	if rc < 0 {
		internal_error();
	}
	rc != 0
}

/// The number of bytes to ask to read from a file at a time.
const CHUNK_SIZE: usize = 16384;

/// Starts reading from a file.
///
/// The `address` parameter identifies the filesystem component by UUID. The `descriptor` parameter
/// is the file descriptor.
///
/// `true` is returned if the call is complete now. `false` is returned if the call has started but
/// will not be complete until the next timeslice.
fn invoke_read(address: &Address, descriptor: descriptor::Borrowed<'_>) -> bool {
	let mut buffer = [0_u8; 13];
	// Write the array header.
	buffer[0] = (4 << 5) | 2;
	// Write the tag.
	buffer[1] = (6 << 5) | 24;
	buffer[2] = 39;
	// Write the descriptor.
	buffer[3] = 26;
	// SAFETY: buffer[4..8] is of length 4. descriptor.to_be_bytes returns 4 bytes because
	// descriptor is a u32. The array is of u8, which impl Copy.
	unsafe {
		let descriptor_bytes: [u8; 4] = descriptor.as_raw().to_be_bytes();
		ptr::copy_nonoverlapping(descriptor_bytes.as_ptr(), buffer[4..8].as_mut_ptr(), 4);
	}
	// Write the requested byte count.
	buffer[8] = 26;
	// SAFETY: buffer[9..13] is of length 4. CHUNK_SIZE.to_be_bytes returns 4 bytes because
	// CHUNK_SIZE is a usize and Wasm is a 32-bit platform. The array is of u8, which impl Copy.
	unsafe {
		let cs_bytes: [u8; 4] = CHUNK_SIZE.to_be_bytes();
		ptr::copy_nonoverlapping(cs_bytes.as_ptr(), buffer[9..13].as_mut_ptr(), 4);
	}
	let method = "read";
	let rc = unsafe {
		component_sys::invoke_component_method(
			address.as_bytes().as_ptr(),
			method.as_ptr(),
			method.len(),
			buffer.as_ptr(),
		)
	};
	// If this fails, it indicates a bug in the BIOS, not a problem with the user’s configuration.
	if rc < 0 {
		internal_error();
	}
	rc != 0
}

/// The type of a bootable medium.
const BOOTABLE_COMPONENT_TYPE: &str = "filesystem";

/// Runs one step of the state machine.
fn run_step(state: State) -> error::Result<(RunResult, State)> {
	// Hold a Lister.
	static mut LISTER: Option<component::Lister> = None;
	// SAFETY: Wasm is single-threaded, so only one thread will be here touching LISTER at a time.
	// This is the only place in which LISTER is touched, so the same thread also cannot make a
	// second mutable reference.
	let lister = unsafe {
		LISTER.get_or_insert_with(|| component::Lister::take().unwrap_or_else(|| internal_error()))
	};

	// Dispatch based on current state.
	match state {
		State::Init => {
			// Find the UUID of the EEPROM.
			let mut listing = lister.start(Some("eeprom"));
			let eeprom = listing
				.next()
				.unwrap_or_else(|| computer::error("BIOS: no EEPROM"));
			let eeprom_uuid = eeprom.address();

			// Call the EEPROM’s “getData” method to read the boot device UUID.
			let method = "getData";
			let rc = unsafe {
				component_sys::invoke_component_method(
					eeprom_uuid.as_bytes().as_ptr(),
					method.as_ptr(),
					method.len(),
					ptr::null(),
				)
			};
			if rc < 0 {
				internal_error();
			}
			Ok((
				if rc == 0 {
					RunResult::Return
				} else {
					RunResult::RunNext
				},
				State::ReadingBootDeviceUuid,
			))
		}
		State::ReadingBootDeviceUuid => {
			// Fetch the call result. An EEPROM’s data area is 256 bytes so 300 should be plenty
			// for the CBOR overhead.
			let mut result_buffer = [0_u8; 300];
			let rc = unsafe {
				component_sys::invoke_end(result_buffer.as_mut_ptr(), result_buffer.len())
			};
			if rc < 0 {
				internal_error();
			}
			// Cast from isize to usize is sound because we just verified rc ≥ 0.
			#[allow(clippy::cast_sign_loss)]
			let result = unsafe { result_buffer.get_unchecked(0..(rc as usize)) };

			// Decode the returned CBOR sequence. We expect a single byte array.
			let (major_type, count, rest) = cbor_decode_header(result)?;
			if major_type != CborMajorType::Array || count != 1 {
				computer::error("BIOS: eeprom.getData bad");
			}
			let (major_type, count, rest) = cbor_decode_header(rest)?;
			if major_type != CborMajorType::Bytes {
				computer::error("BIOS: eeprom.getData bad");
			}
			if rest.len() as u64 != count {
				computer::error("BIOS: eeprom.getData bad");
			}

			// Check if it’s a binary UUID address. If not, don’t explode, just skip straight to
			// scanning for a bootable medium.
			if let Ok(boot_device) = rest.try_into().map(Address::from_bytes) {
				// Check whether the specified component exists and, if so, is of type
				// filesystem.
				let mut boot_device_type_buffer = [0_u8; BOOTABLE_COMPONENT_TYPE.len()];
				// component_type can fail for reasons BufferTooShort or NoSuchComponent. The
				// buffer is long enough to hold the component type we care about,so either of
				// those means the boot device is either not found or is not a filesystem. In
				// those cases, skip to scanning.
				if let Ok(candidate_type) =
					component::component_type(&boot_device, &mut boot_device_type_buffer)
				{
					if candidate_type == BOOTABLE_COMPONENT_TYPE {
						let done = invoke_open_init(&boot_device);
						return Ok((
							if done {
								RunResult::RunNext
							} else {
								RunResult::Return
							},
							State::OpeningFile(OpeningFileInfo {
								uuid: boot_device,
								source: UuidSource::Eeprom,
							}),
						));
					}
				}
			}

			// We couldn’t a designated boot device (either there wasn’t one, or it doesn’t exist,
			// or it isn’t a filesystem). Start a scan.
			Ok((RunResult::RunNext, State::StartScan))
		}
		State::StartScan => {
			// List all components of the proper type and start opening init.wasm on the first one.
			let listing = lister.start(Some(BOOTABLE_COMPONENT_TYPE));
			Ok((RunResult::RunNext, State::Scanning(listing)))
		}
		State::Scanning(mut listing) => {
			// Fetch the next component in the list.
			if let Some(entry) = listing.next() {
				// We found a component. Try opening /init.wasm on it.
				let done = invoke_open_init(entry.address());
				Ok((
					if done {
						RunResult::RunNext
					} else {
						RunResult::Return
					},
					State::OpeningFile(OpeningFileInfo {
						uuid: *entry.address(),
						source: UuidSource::Scan(listing),
					}),
				))
			} else {
				// There are no more components.
				computer::error("BIOS: no bootable medium")
			}
		}
		State::OpeningFile(info) => {
			// Fetch the call result. An open call returns either a handle or else a null followed
			// by the filename you tried to open, so make a buffer large enough to hold either of
			// those.
			let mut result_buffer = [0_u8; 32 + FILENAME.len()];
			let rc = unsafe {
				component_sys::invoke_end(result_buffer.as_mut_ptr(), result_buffer.len())
			};
			if rc >= 0 {
				// Decode the first data item.
				// Cast from isize to usize is sound because we just verified rc ≥ 0.
				#[allow(clippy::cast_sign_loss)]
				let result = unsafe { result_buffer.get_unchecked(0..(rc as usize)) };
				let (major, count, rest) = cbor_decode_header(result)?;
				if major == CborMajorType::Array && count == 1 {
					let (major, count, rest) = cbor_decode_header(rest)?;
					if major == CborMajorType::Tag && count == 39 {
						// This is an Identifier tag. Its payload remains, and is the tagged data item.
						let (major, count, _) = cbor_decode_header(rest)?;
						if major == CborMajorType::UnsignedInteger {
							// We got a file descriptor. Read the file.
							// Cast from u64 to u32 is sound because descriptors are always small.
							#[allow(clippy::cast_possible_truncation)]
							let descriptor = count as u32;
							// SAFETY: We just saw an Identifier (39) tagged integer in CBOR data
							// provided by OC-Wasm. That can only appear when handing over a fresh
							// descriptor.
							let descriptor = unsafe { descriptor::Owned::new(descriptor) };
							let done = invoke_read(&info.uuid, descriptor.as_descriptor());
							Ok((
								if done {
									RunResult::RunNext
								} else {
									RunResult::Return
								},
								State::ReadingFile(ReadingFileInfo {
									uuid: info.uuid,
									descriptor,
								}),
							))
						} else {
							computer::error("BIOS: filesystem.open bad")
						}
					} else {
						computer::error("BIOS: filesystem.open bad")
					}
				} else {
					computer::error("BIOS: filesystem.open bad")
				}
			} else if rc == -12
			/* Other error */
			{
				// This probably means open failed. Scan or continue scanning for other
				// bootable media.
				Ok((
					RunResult::RunNext,
					match info.source {
						UuidSource::Eeprom => State::StartScan,
						UuidSource::Scan(listing) => State::Scanning(listing),
					},
				))
			} else {
				computer::error("BIOS: filesystem.open bad")
			}
		}
		State::ReadingFile(info) => {
			// Fetch the call result.
			let mut result_buffer = [0_u8; 32 + CHUNK_SIZE];
			let rc = unsafe {
				component_sys::invoke_end(result_buffer.as_mut_ptr(), result_buffer.len())
			};
			if rc < 0 {
				internal_error();
			}
			// Cast from isize to usize is sound because we just verified rc ≥ 0.
			#[allow(clippy::cast_sign_loss)]
			let result = unsafe { result_buffer.get_unchecked(0..(rc as usize)) };
			// Decode the first data item.
			let (major, count, rest) = cbor_decode_header(result)?;
			if major == CborMajorType::Array && count == 1 {
				let (major, count, rest) = cbor_decode_header(rest)?;
				if major == CborMajorType::Bytes && count <= rest.len() as u64 {
					// We got some file data. Add it to the execution buffer and try to get some more.
					// SAFETY: we just checked that count ≤ rest.len()
					// Cast from u64 to usize is sound because count ≤ rest.len().
					#[allow(clippy::cast_possible_truncation)]
					execute::add(unsafe { rest.get_unchecked(0..count as usize) })?;
					let done = invoke_read(&info.uuid, info.descriptor.as_descriptor());
					Ok((
						if done {
							RunResult::RunNext
						} else {
							RunResult::Return
						},
						State::ReadingFile(info),
					))
				} else if major == CborMajorType::Special && count == 22 {
					// We got null, indicating EOF.
					drop(info);
					execute::execute()
				} else {
					// We got something unexpected.
					computer::error("BIOS: I/O error reading /init.wasm")
				}
			} else {
				// We did not get a 1-element array.
				computer::error("BIOS: I/O error reading /init.wasm")
			}
		}
	}
}

/// The application entry point.
#[no_mangle]
pub extern "C" fn run(_: i32) -> i32 {
	static mut STATE: State = State::Init;

	// Run continuously until asked to return.
	loop {
		// SAFETY: Wasm is single-threaded, so only one thread can be here at a time. The mutable
		// reference to STATE lasts only for the duration of the replace() call.
		let old_state = replace(unsafe { &mut STATE }, State::Init);
		let rc: error::Result<(RunResult, State)> = run_step(old_state);
		match rc {
			Ok((result, next_state)) => {
				// SAFETY: Wasm is single-threaded, so only one thread can be here at a time. The
				// only reference to STATE exists a few lines above in the replace() call, and is
				// long dead by now.
				unsafe { STATE = next_state };
				match result {
					RunResult::RunNext => (),
					RunResult::Return => return 0,
				}
			}
			Err(_) => computer::error("BIOS: internal error"),
		}
	}
}
