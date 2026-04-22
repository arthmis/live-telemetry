use std::mem;
use windows::Win32::{
    Foundation::CloseHandle,
    System::Memory::{FILE_MAP_READ, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile},
};

/// Mirrors the first three fields of `SPageFilePhysics`.
///
/// The struct uses `#pragma pack(4)` and every field is 4 bytes wide, so the
/// layout is a flat byte blob with no padding:
///
/// | offset | type | field      |
/// |--------|------|------------|
/// |  0     | i32  | packet_id  |
/// |  4     | f32  | gas        |
/// |  8     | f32  | brake      |
#[repr(C)]
struct PhysicsPage {
    packet_id: i32,
    /// Throttle input in the range `0.0–1.0`.
    throttle: f32,
    /// Brake input in the range `0.0–1.0`.
    brake: f32,
}

/// Opens `Local\acevo_pmf_physics`, copies the first twelve bytes into a
/// [`PhysicsPage`], and returns `(throttle, brake)`.
///
/// Returns a [`windows::core::Error`] when the game is not running or the
/// shared-memory page has not yet been created.
fn read_inputs() -> windows::core::Result<PhysicsPage> {
    // The map name is a UTF-16 wide string.  The `w!` macro appends the null
    // terminator at compile time.
    let map_name = windows::core::w!("Local\\acevo_pmf_physics");

    unsafe {
        // 1. Obtain a handle to the existing file mapping created by AC Evo.
        let handle = OpenFileMappingW(FILE_MAP_READ.0, false, map_name)?;

        // 2. Map the first sizeof(PhysicsPage) bytes into our address space.
        let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, mem::size_of::<PhysicsPage>());

        if view.Value.is_null() {
            // MapViewOfFile failed – close the handle before propagating the error.
            let err = windows::core::Error::from_thread();
            let _ = CloseHandle(handle);
            return Err(err);
        }

        // 3. Copy the data out before unmapping so we do not hold a dangling
        //    pointer to the mapped region.
        let page = std::ptr::read(view.Value as *const PhysicsPage);

        // 4. Clean up in reverse order.
        UnmapViewOfFile(view)?;
        CloseHandle(handle)?;

        Ok(page)
    }
}

fn main() {
    match read_inputs() {
        Ok(PhysicsPage {
            throttle, brake, ..
        }) => {
            println!("throttle: {:.3}  brake: {:.3}", throttle, brake);
        }
        Err(e) => {
            eprintln!("Failed to read shared memory: {e}");
            eprintln!("Is Assetto Corsa Evo running?");
        }
    }
}
