use serde::{Deserialize, Serialize};
use shared_memory::{Shmem, ShmemConf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{System};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::env;
use log;
use std::fs::File;
use std::path::Path;
use std::time::Duration;
use once_cell::sync::Lazy;
use simplelog::{WriteLogger, LevelFilter, Config};

const MAX_SLOTS: usize = 99;
const SHARED_MEM_NAME: &str = "slot_allocator";
const MAGIC_NUMBER: u32 = 0xFEEEEEED;
// Add a safety margin to the shared memory size
const SHARED_MEM_PADDING: usize = 1024;  // 1KB padding
const MAX_LOCK_ATTEMPTS:usize = 25;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(10);
const HEADER_SIZE: usize = 8;
const DATA_SIZE: usize = 4096;
const SHARED_MEM_SIZE: usize = HEADER_SIZE + DATA_SIZE;
pub static GLOBAL_ALLOCATOR: Lazy<Mutex<Option<SlotAllocator>>> = Lazy::new(|| {
    Mutex::new(None)
});


#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
struct SlotInfo {
    process_id: u32,
    last_heartbeat: u64,
}

#[repr(C)]
struct SharedMemoryLayout {
    magic: u32,
    lock: u8,
    state: SharedState,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct SharedState {
    slots: [Option<SlotInfo>; MAX_SLOTS],
}

impl Default for SharedState {
    fn default() -> Self {
        SharedState {
            slots: [None; MAX_SLOTS],
        }
    }
}

// Rest of the implementation remains the same, starting from here:
pub struct SlotAllocator {
    shm: Shmem,
    allocated_slot: Option<usize>,
}

unsafe impl Send for SlotAllocator {}

#[derive(Debug)]
pub enum SlotError {
    Lock(String),
    SharedMemory(String),
    InvalidSlot(String),
    InvalidMagic(String),
}

impl std::fmt::Display for SlotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotError::Lock(msg) => write!(f, "Lock error: {}", msg),
            SlotError::SharedMemory(msg) => write!(f, "Shared memory error: {}", msg),
            SlotError::InvalidSlot(msg) => write!(f, "Invalid slot error: {}", msg),
            SlotError::InvalidMagic(msg) => write!(f, "Invalid magic number: {}", msg),
        }
    }
}

impl std::error::Error for SlotError {}

impl Drop for SlotAllocator {
    fn drop(&mut self) {
        if let Some(slot) = self.allocated_slot {
            let _ = self.release_slot(slot);
        }
    }
}

impl SlotAllocator {
    fn get_shared_memory_path() -> String {
        if cfg!(windows) {
            format!("{}\\{}",
                std::env::var("TEMP").unwrap_or(String::from("C:\\Windows\\Temp")),
                SHARED_MEM_NAME
            )
        } else if cfg!(target_os = "macos") {
            format!("/tmp/{}", SHARED_MEM_NAME)
        } else {
            format!("/dev/shm/{}", SHARED_MEM_NAME)
        }
    }

    pub fn new() -> Result<Self, SlotError> {
        log::info!("Creating new SlotAllocator");
        log::info!("Shared memory path: {}", Self::get_shared_memory_path());
        let shm = match ShmemConf::new()
            .size(SHARED_MEM_SIZE)
            .os_id(SHARED_MEM_NAME)
            .create()
        {
            Ok(mem) => {
                log::info!("Created new shared memory");
                mem
            },
            Err(_) => {
                log::info!("Opening existing shared memory");
                ShmemConf::new()
                    .os_id(SHARED_MEM_NAME)
                    .open()
                    .map_err(|e| SlotError::SharedMemory(e.to_string()))?
            }
        };

        if shm.is_owner() {
            log::info!("Initializing shared memory as owner");
            let layout = SharedMemoryLayout {
                magic: MAGIC_NUMBER,
                lock: 0,
                state: SharedState::default(),
            };

            unsafe {
                *(shm.as_ptr() as *mut SharedMemoryLayout) = layout;
            }
            log::info!("Initialized shared memory");
        }

        Ok(SlotAllocator { 
            shm,
            allocated_slot: None,
        })
    }

    fn get_layout(&self) -> Result<&mut SharedMemoryLayout, SlotError> {
        unsafe {
            let layout = &mut *(self.shm.as_ptr() as *mut SharedMemoryLayout);
            if layout.magic != MAGIC_NUMBER {
                log::info!("Invalid magic number detected (0x{:X}), reinitializing shared memory", layout.magic);
                // Reinitialize the memory
                *layout = SharedMemoryLayout {
                    magic: MAGIC_NUMBER,
                    lock: 0,
                    state: SharedState::default(),
                };
            }
            Ok(layout)
        }
    }

    pub fn init_global() -> Result<(), SlotError> {
        log::info!("Initializing global allocator");
        let mut global = GLOBAL_ALLOCATOR.lock().unwrap();
        if global.is_none() {
            *global = Some(SlotAllocator::new()?);
            log::info!("Global allocator initialized");
        } 
        Ok(())
    }

    fn acquire_lock(&self) -> Result<named_lock::NamedLockGuard, SlotError> {
        let mut attempts = 0;
        while attempts < 50 {

            if let Ok(lock) = named_lock::NamedLock::create("instancifier") {
                if let Ok(guard) = lock.try_lock() {
                    return Ok(guard)
                }
            }

            attempts += 1;
            thread::sleep(Duration::from_millis(100));
        }
        Err(SlotError::Lock("Failed to acquire lock".into()))
    }

    fn is_process_running(pid: u32) -> bool {
        let mut system = System::new_all();
        system.refresh_all();

        system.process(sysinfo::Pid::from_u32(pid)).is_some()
    }

    pub fn allocate_slot(&mut self) -> Result<Option<usize>, SlotError> {
        log::info!("Attempting to allocate slot");
        if self.allocated_slot.is_some() {
            log::info!("Already have slot {:?}", self.allocated_slot);
            return Ok(self.allocated_slot);
        }

        let _lock_guard = self.acquire_lock()?;
        log::info!("Lock acquired");

        let result = (|| {
            let layout = self.get_layout()?;
            let current_time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| SlotError::SharedMemory(e.to_string()))?
                .as_secs();

            // Clean up abandoned slots
            for (i, slot) in layout.state.slots.iter_mut().enumerate() {
                if let Some(info) = slot {
                    if !Self::is_process_running(info.process_id) {
                        log::info!("Cleaned up abandoned slot {} from PID {}", i + 1, info.process_id);
                        *slot = None;
                    }
                }
            }

            log::info!("Searching for empty slot");
            let empty_slots = layout.state.slots.iter().filter(|s| s.is_none()).count();
            log::info!("Found {} empty slots", empty_slots);

            let slot_index = layout.state.slots.iter().position(|slot| slot.is_none());
            log::info!("Position of first empty slot: {:?}", slot_index);
            
            if let Some(index) = slot_index {
                log::info!("Allocating slot {}", index + 1);
                layout.state.slots[index] = Some(SlotInfo {
                    process_id: std::process::id(),
                    last_heartbeat: current_time,
                });
                self.allocated_slot = Some(index + 1);
                Ok(Some(index + 1))
            } else {
                log::info!("No empty slots available");
                Ok(None)
            }
        })();
        result
    }

    pub fn print_status(&self) -> Result<(), SlotError> {
        let _lock_guard = self.acquire_lock()?;
        
        let result = (|| {
            let layout = self.get_layout()?;
            println!("\nCurrent Slot Allocation Status:");
            println!("------------------------------");
            println!("Magic: 0x{:X}", layout.magic);
            
            for (i, slot) in layout.state.slots.iter().enumerate() {
                match slot {
                    Some(info) => {
                        let process_status = if Self::is_process_running(info.process_id) {
                            "RUNNING"
                        } else {
                            "DEAD"
                        };
                        println!(
                            "Slot {:2}: PID {:6} ({}) {}", 
                            i + 1, 
                            info.process_id, 
                            process_status,
                            if Some(i + 1) == self.allocated_slot { "*" } else { " " }
                        );
                    }
                    None => println!("Slot {:2}: Available", i + 1),
                }
            }
            println!("------------------------------");
            Ok(())
        })();

        result
    }

    pub fn release_slot(&mut self, slot_number: usize) -> Result<(), SlotError> {
        if slot_number == 0 || slot_number > MAX_SLOTS {
            return Err(SlotError::InvalidSlot("Invalid slot number".into()));
        }

        let _lock_guard = self.acquire_lock()?;

        let result = (|| {
            let layout = self.get_layout()?;
            let index = slot_number - 1;

            if let Some(info) = &layout.state.slots[index] {
                if info.process_id == std::process::id() {
                    layout.state.slots[index] = None;
                    self.allocated_slot = None;
                    log::info!("Released slot {}", slot_number);
                    Ok(())
                } else {
                    Err(SlotError::InvalidSlot("Cannot release slot owned by another process".into()))
                }
            } else {
                Ok(())
            }
        })();
        result
    }
}

#[no_mangle]
pub extern "C" fn dump_slots() -> i32{
    if let Err(e) = SlotAllocator::init_global() {
        log::info!("{:?}", e);
        return -1;
    }


    // Use the global allocator
    let mut allocator = GLOBAL_ALLOCATOR.lock().unwrap();
    if let Some(allocator) = allocator.as_mut() {

        if let Err(e) = allocator.print_status() {
            log::info!("{:?}", e);
            return -1;
        }
    } 
    return 0;
}

#[no_mangle]
pub extern "C" fn free_slot(slot: usize) -> i32{
    if let Err(e) = SlotAllocator::init_global() {
        log::info!("{:?}", e);
        return -1;
    }


    // Use the global allocator
    let mut allocator = GLOBAL_ALLOCATOR.lock().unwrap();
    if let Some(allocator) = allocator.as_mut() {

        if let Err(e) = allocator.release_slot(slot) {
            log::info!("{:?}", e);
            return -1;
        }
    } 
    return 0;
}

#[no_mangle]
pub extern "C" fn find_available_slot() -> i32 {
    
    if let Ok(log_path) = env::var("INSTANCIFIER_LOG_PATH") {

        let file_path = Path::new(log_path.as_str());
        if let Some(parent_dir) = file_path.parent() {
            if parent_dir.exists() {
                WriteLogger::init(
                    LevelFilter::Info,
                    Config::default(),
                    File::create(log_path).unwrap()
                ).unwrap();
            } 
        }
    }
    
    
    if let Err(e) = SlotAllocator::init_global() {
        log::info!("{:?}", e);
        return -1;
    } else {
        log::info!("global initialized");
    }


    // Use the global allocator
    let mut allocator = GLOBAL_ALLOCATOR.lock().unwrap();
    if let Some(allocator) = allocator.as_mut() {

        match allocator.allocate_slot() {
            Ok(Some(slot)) => { 
                slot as i32
            },
            Ok(None) => {
                log::info!("no slot found but no error reported");
                -1
            },
            Err(e) => {
                log::info!("{:?}", e);
                -1
            }
        }
    } else {
        -1 
    }
}
