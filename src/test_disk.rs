//! Disk checks are defined here.  To define a new
//! check create a struct and impl the DiskCheck trait.
//! To create a remediation should that check fail you
//! should also impl the DiskRemediation trait.
//!
//!                            +------>disk_is_ok           +----->replace_disk
//!                            + no                         +no
//!       +---->is_filesystem_corrupted      +--------> can_i_repair
//!       + no                 + yes         + no      ^   + yes
//!is_disk_writable            +------>is_mounted      |   +----->repair_disk
//!       + yes                              + yes     +
//!       +----->disk_is_ok                  +---->unmount
//extern crate blkid;
extern crate block_utils;
extern crate fstab;
extern crate libatasmart;
extern crate log;
#[cfg(test)]
extern crate mocktopus;
extern crate petgraph;
extern crate rayon;
extern crate rusqlite;
extern crate tempdir;

use in_progress;

use self::block_utils::{
    format_block_device, get_mountpoint, is_mounted, mount_device, Device, Filesystem,
    FilesystemType, MediaType,
};
use self::in_progress::*;
#[cfg(test)]
use self::mocktopus::macros::*;
use self::petgraph::graphmap::GraphMap;
use self::petgraph::Directed;
use self::rayon::prelude::*;
use self::rusqlite::Connection;
use self::tempdir::TempDir;

use std::fmt;
use std::fs::OpenOptions;
use std::io::{Error, ErrorKind};
use std::io::{Result, Write};
use std::path::Path;
use std::process::Command;
use std::str::FromStr;

#[cfg(test)]
mod tests {
    extern crate blkid;
    extern crate simplelog;
    extern crate uuid;

    use in_progress;

    use std::fs::{remove_file, File};
    use std::io::{Error, ErrorKind, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use self::blkid::BlkId;
    use self::uuid::Uuid;
    //    use super::mocktopus::macros::*;
    use super::mocktopus::mocking::*;
    use simplelog::{Config, TermLogger};

    fn create_loop_device() -> PathBuf {
        // Find free loopback device
        let out = Command::new("losetup").args(&["-f"]).output().unwrap();
        // Assert we created the device
        assert_eq!(out.status.success(), true);

        let stdout = String::from_utf8_lossy(&out.stdout);
        let free_device = stdout.trim();

        // Create a loopback device for testing
        let mut f = File::create("/tmp/file.img").unwrap();
        // Write 25MB to a file
        debug!("writing 25MB to /tmp/file.img");
        let buff = [0x00; 1024];
        for _ in 0..25600 {
            f.write(&buff).unwrap();
        }
        f.sync_all().unwrap();

        // Setup a loopback device for testing filesystem corruption
        debug!("setting up {} device", free_device);
        Command::new("losetup")
            .args(&[free_device, "/tmp/file.img"])
            .status()
            .unwrap();

        // Put an xfs filesystem down on it
        debug!("Putting xfs on to {}", free_device);
        Command::new("mkfs.xfs")
            .args(&[free_device])
            .status()
            .unwrap();

        PathBuf::from(free_device)
    }

    fn cleanup_loop_device(p: &Path) {
        // Cleanup
        Command::new("umount")
            .args(&[&p.to_string_lossy().into_owned()])
            .status()
            .unwrap();

        Command::new("losetup")
            .args(&["-d", &p.to_string_lossy()])
            .status()
            .unwrap();
    }

    #[test]
    fn test_state_machine_base() {
        TermLogger::init(super::log::LevelFilter::Debug, Config::default()).unwrap();

        // Mock smart to return Ok(true)
        super::run_smart_checks.mock_safe(|_| MockResult::Return(Ok(true)));

        let dev = create_loop_device();

        let blkid = BlkId::new(&dev).unwrap();
        blkid.do_probe().unwrap();
        let drive_uuid = blkid.lookup_value("UUID").unwrap();
        debug!("drive_uuid: {}", drive_uuid);

        let drive_id = Uuid::parse_str(&drive_uuid).unwrap();
        // Cleanup previous test runs
        remove_file("/tmp/db.sqlite3").unwrap();
        let conn = super::connect_to_repair_database(Path::new("/tmp/db.sqlite3")).unwrap();

        let d = super::Device {
            id: Some(drive_id),
            name: dev.file_name().unwrap().to_str().unwrap().to_string(),
            media_type: super::MediaType::Rotational,
            capacity: 26214400,
            fs_type: super::FilesystemType::Xfs,
            serial_number: Some("123456".into()),
        };
        let mut s = super::StateMachine::new(d, conn, true);
        s.setup_state_machine();
        s.print_graph();
        s.restore_state().unwrap();
        s.run();
        println!("final state: {}", s.state);

        cleanup_loop_device(&dev);

        assert_eq!(s.state, super::State::Good);
    }

    #[test]
    fn test_state_machine_bad_filesystem() {
        TermLogger::init(super::log::LevelFilter::Debug, Config::default()).unwrap();

        // Mock smart to return Ok(true)
        super::run_smart_checks.mock_safe(|_| MockResult::Return(Ok(true)));

        let dev = create_loop_device();
        let blkid = BlkId::new(&dev).unwrap();
        blkid.do_probe().unwrap();
        let drive_uuid = blkid.lookup_value("UUID").unwrap();
        debug!("drive_uuid: {}", drive_uuid);

        debug!("Corrupting the filesystem");
        // This is repairable by xfs_repair
        Command::new("xfs_db")
            .args(&[
                "-x",
                "-c",
                "blockget",
                "-c",
                "blocktrash",
                &dev.to_string_lossy().into_owned(),
            ]).status()
            .unwrap();

        let drive_id = Uuid::parse_str(&drive_uuid).unwrap();
        // Cleanup previous test runs
        remove_file("/tmp/db.sqlite3").unwrap();
        let conn = super::connect_to_repair_database(Path::new("/tmp/db.sqlite3")).unwrap();
        let d = super::Device {
            id: Some(drive_id),
            name: dev.file_name().unwrap().to_str().unwrap().to_string(),
            media_type: super::MediaType::Rotational,
            capacity: 26214400,
            fs_type: super::FilesystemType::Xfs,
            serial_number: Some("123456".into()),
        };
        let mut s = super::StateMachine::new(d, conn, true);
        s.setup_state_machine();
        s.print_graph();
        s.restore_state().unwrap();
        s.run();
        println!("final state: {}", s.state);

        cleanup_loop_device(&dev);
        assert_eq!(s.state, super::State::Good);
    }

    #[test]
    fn test_state_machine_replace_disk() {
        // Smart passes, write fails,  check_filesystem fails, attemptRepair and reformat fails
        TermLogger::init(super::log::LevelFilter::Debug, Config::default()).unwrap();

        super::run_smart_checks.mock_safe(|_| MockResult::Return(Ok(true)));
        super::check_writable.mock_safe(|_| MockResult::Return(Err(Error::new(ErrorKind::Other, "Mock Error"))));
        super::check_filesystem
            .mock_safe(|_, _| MockResult::Return(Err(Error::new(ErrorKind::Other, "Mock Error"))));
        super::repair_filesystem
            .mock_safe(|_, _| MockResult::Return(Err(Error::new(ErrorKind::Other, "Mock Error"))));
        super::format_block_device.mock_safe(|_, _| MockResult::Return(Err("error".to_string())));
        // That should leave the disk in WaitingForReplacement

        let dev = create_loop_device();

        let blkid = BlkId::new(&dev).unwrap();
        blkid.do_probe().unwrap();
        let drive_uuid = blkid.lookup_value("UUID").unwrap();
        debug!("drive_uuid: {}", drive_uuid);

        let drive_id = Uuid::parse_str(&drive_uuid).unwrap();
        // Cleanup previous test runs
        remove_file("/tmp/db.sqlite3").unwrap();
        let conn = super::connect_to_repair_database(Path::new("/tmp/db.sqlite3")).unwrap();

        let d = super::Device {
            id: Some(drive_id),
            name: dev.file_name().unwrap().to_str().unwrap().to_string(),
            media_type: super::MediaType::Rotational,
            capacity: 26214400,
            fs_type: super::FilesystemType::Xfs,
            serial_number: Some("123456".into()),
        };
        let mut s = super::StateMachine::new(d, conn, true);
        s.setup_state_machine();
        s.print_graph();
        s.restore_state().unwrap();
        s.run();
        println!("final state: {}", s.state);

        cleanup_loop_device(&dev);

        assert_eq!(s.state, super::State::WaitingForReplacement);
    }

    #[test]
    fn test_state_machine_replaced_disk() {}

    #[test]
    fn test_state_machine_resume() {
        TermLogger::init(super::log::LevelFilter::Debug, Config::default()).unwrap();

        let drive_id = Uuid::parse_str("6eab3005-73a8-4287-b6c6-b83e1def469a").unwrap();
        // Cleanup previous test runs
        remove_file("/tmp/db.sqlite3").unwrap();
        let conn = super::connect_to_repair_database(Path::new("/tmp/db.sqlite3")).unwrap();

        // Set the previous state to something other than Unscanned
        in_progress::save_state(
            &conn,
            Path::new("/dev/sda"),
            super::State::WaitingForReplacement,
        ).unwrap();

        let d = super::Device {
            id: Some(drive_id),
            name: "sda".to_string(),
            media_type: super::MediaType::Rotational,
            capacity: 1024,
            fs_type: super::FilesystemType::Xfs,
            serial_number: Some("123456".into()),
        };
        let mut s = super::StateMachine::new(d, conn, true);
        s.setup_state_machine();
        s.print_graph();
        s.restore_state().unwrap();
        s.run();
        println!("final state: {}", s.state);
    }

}

trait Transition {
    // Transition from the current state to an ending state given an Event
    // database connection can be used to save and resume state
    fn transition(
        from_state: &State,
        to_state: &State,
        device: &Device,
        db_conn: &Connection,
        simulate: bool, // Pretend to transition and skip any side effects
    ) -> State;
}

impl Transition for AttemptRepair {
    // Take a Corrupt
    fn transition(
        from_state: &State,
        to_state: &State,
        device: &Device,
        _db_conn: &Connection,
        simulate: bool,
    ) -> State {
        debug!("running AttemptRepair transition");
        // TODO: This information shouldn't be stored here
        if from_state != &State::Corrupt {
            debug!("Skipping AttemptRepair transition");
            return to_state.clone();
        }
        // Disk filesystem is corrupted.  Attempt repairs.
        let tmp = format!("/dev/{}", device.name);
        let dev_path = Path::new(&tmp);
        if !simulate {
            match repair_filesystem(&device.fs_type, &dev_path) {
                Ok(_) => to_state.clone(),
                Err(e) => {
                    error!("repair_filesystem failed on {:?}: {}", device, e);
                    State::RepairFailed
                }
            }
        } else {
            to_state.clone()
        }
    }
}

impl Transition for CheckForCorruption {
    fn transition(
        from_state: &State,
        to_state: &State,
        device: &Device,
        _db_conn: &Connection,
        simulate: bool,
    ) -> State {
        debug!("running CheckForCorruption transition");
        // TODO: This information shouldn't be stored here
        if from_state != &State::Scanned {
            debug!("Skipping CheckForCorruption transition");
            return to_state.clone();
        }
        let tmp = format!("/dev/{}", device.name);
        let dev_path = Path::new(&tmp);
        if !simulate {
            match check_filesystem(&device.fs_type, &dev_path) {
                Ok(_) => to_state.clone(),
                Err(e) => {
                    error!("check_filesystem failed on {:?}: {}", device, e);
                    State::Fail
                }
            }
        } else {
            to_state.clone()
        }
    }
}

impl Transition for CheckWearLeveling {
    fn transition(
        from_state: &State,
        to_state: &State,
        _device: &Device,
        _db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running CheckWearLeveling transition");
        // TODO: This information shouldn't be stored here
        if from_state != &State::Scanned {
            debug!("Skipping CheckWearLeveling transition");
            return to_state.clone();
        }

        //TODO: How can we check wear leveling?
        to_state.clone()
    }
}

// Evaluate whether a scanned drive is good
impl Transition for Eval {
    fn transition(
        from_state: &State,
        to_state: &State,
        device: &Device,
        db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running Eval transition");
        // TODO: This information shouldn't be stored here
        if from_state != &State::Scanned {
            debug!("Skipping Eval transition");
            return to_state.clone();
        }
        let tmp = format!("/dev/{}", device.name);
        let dev_path = Path::new(&tmp);

        let mnt_dir: TempDir;
        if !is_mounted(&dev_path).unwrap_or(false) {
            debug!("Mounting device: {}", dev_path.display());
            mnt_dir = match TempDir::new("bynar") {
                Ok(d) => d,
                Err(e) => {
                    error!("temp dir creation failed: {:?}", e);
                    return State::Fail;
                }
            };
            // This requires root perms
            if let Err(e) = mount_device(&device, &mnt_dir.path().to_string_lossy()) {
                error!("Mounting {} failed: {}", dev_path.display(), e);
                return State::MountFailed;
            }
        }

        debug!("Getting mountpoint info for {}", dev_path.display());
        match get_mountpoint(&dev_path) {
            Ok(mount_info) => match mount_info {
                Some(info) => {
                    debug!("mount info: {:?}", info);
                    if let Err(e) = save_mount_location(&db_conn, &dev_path, &info) {
                        error!(
                            "save mount location failed for {}: {:?}",
                            dev_path.display(),
                            e
                        );
                        return State::Fail;
                    }

                    match check_writable(&info) {
                        // Mount point is writeable, smart passed.  Good to go
                        Ok(_) => to_state.clone(),
                        Err(e) => {
                            //Should proceed to error checking now
                            error!("Error writing to disk: {:?}", e);
                            State::WriteFailed
                        }
                    }
                }
                None => {
                    // Device isn't mounted.  Mount in temp location and check?
                    // what if it doesn't have a filesystem.

                    // This shouldn't happen because !is_mounted above
                    // took care of it
                    error!("Device is not mounted");
                    State::NotMounted
                }
            },
            Err(e) => {
                error!(
                    "Error getting mountpoint for {}: {:?}",
                    dev_path.display(),
                    e
                );
                State::Fail
            }
        }
    }
}

impl Transition for MarkForReplacement {
    fn transition(
        from_state: &State,
        to_state: &State,
        device: &Device,
        db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running MarkForReplacement transition");
        // TODO: This information shouldn't be stored here
        if from_state != &State::WornOut || from_state != &State::Corrupt {
            debug!("Skipping MarkForReplacement transition");
            return to_state.clone();
        }
        let tmp = format!("/dev/{}", device.name);
        let dev_path = Path::new(&tmp);
        match is_disk_in_progress(&db_conn, &dev_path) {
            Ok(in_progress) => {
                if in_progress {
                    // This is already in waiting for replacement
                    to_state.clone()
                } else {
                    // TODO: Does this make sense?
                    to_state.clone()
                }
            }
            Err(e) => {
                error!(
                    "Error getting disk progress for {}: {:?}",
                    dev_path.display(),
                    e
                );
                State::Fail
            }
        }
    }
}

impl Transition for Mount {
    fn transition(
        _from_state: &State,
        to_state: &State,
        device: &Device,
        _db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running mount transition");

        let tmp = format!("/dev/{}", device.name);
        let dev_path = Path::new(&tmp);
        let mnt_dir: TempDir;

        debug!("Mounting device: {}", dev_path.display());
        mnt_dir = match TempDir::new("bynar") {
            Ok(d) => d,
            Err(e) => {
                error!("temp dir creation failed: {:?}", e);
                return State::Fail;
            }
        };
        if let Err(e) = mount_device(&device, &mnt_dir.path().to_string_lossy()) {
            error!("Mounting {} failed: {}", dev_path.display(), e);
            return State::MountFailed;
        }

        to_state.clone()
    }
}

impl Transition for NoOp {
    fn transition(
        _from_state: &State,
        to_state: &State,
        _device: &Device,
        _db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running NoOp transition");

        to_state.clone()
    }
}

impl Transition for Reformat {
    fn transition(
        _from_state: &State,
        to_state: &State,
        device: &Device,
        _db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running Reformat transition");
        let tmp = format!("/dev/{}", device.name);
        let dev_path = Path::new(&tmp);

        match format_block_device(&dev_path, &Filesystem::new(device.fs_type.to_str())) {
            Ok(_) => to_state.clone(),
            Err(e) => {
                error!("Reformat failed: {}", e);
                State::ReformatFailed
            }
        }
    }
}

impl Transition for Remount {
    fn transition(
        _from_state: &State,
        to_state: &State,
        _device: &Device,
        _db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running Remount transition");
        // TODO: Investigate using libmount here
        match Command::new("mount").args(&["-o", "remount"]).output() {
            Ok(output) => {
                if output.status.success() {
                    to_state.clone()
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    error!("Remount failed: {}", stderr);
                    State::MountFailed
                }
            }
            Err(e) => {
                error!("Remount failed: {}", e);
                State::MountFailed
            }
        }
    }
}

impl Transition for Replace {
    fn transition(
        from_state: &State,
        to_state: &State,
        _device: &Device,
        _db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running Replace transition");
        // TODO: This information shouldn't be stored here
        if from_state != &State::WaitingForReplacement {
            debug!("Skipping Replace transition");
            return to_state.clone();
        }
        // So we know at this point that the disk has been replaced
        // We know the device we're working with

        to_state.clone()
    }
}

impl Transition for Scan {
    fn transition(
        from_state: &State,
        to_state: &State,
        device: &Device,
        db_conn: &Connection,
        _simulate: bool,
    ) -> State {
        debug!("running Scan transition");
        // TODO: This information shouldn't be stored here
        if from_state != &State::Unscanned {
            debug!("Skipping Scan transition");
            return to_state.clone();
        }

        // TODO: Handle devices that live behind a raid controller
        let dev_path = format!("/dev/{}", device.name);
        // Run a smart check on the base device without partition
        match run_smart_checks(&Path::new(&dev_path)) {
            Ok(_) => match save_smart_results(&db_conn, &Path::new(&dev_path), true) {
                Ok(_) => to_state.clone(),
                Err(e) => {
                    error!("Save smart results failed {:?}", e);
                    State::Fail
                }
            },
            Err(e) => {
                error!("Smart test failed: {:?}", e);
                match save_smart_results(&db_conn, &Path::new(&dev_path), false) {
                    Ok(_) => State::Fail,
                    Err(e) => {
                        error!("Save smart results failed {:?}", e);
                        State::Fail
                    }
                }
            }
        }
    }
}

pub struct StateMachine {
    // Mapping of valid From -> To transitions
    graph: GraphMap<
        State,
        fn(
            from_state: &State,
            to_state: &State,
            device: &Device,
            db_conn: &Connection,
            simulate: bool,
        ) -> State,
        Directed,
    >,
    pub state: State,
    pub disk: Device,
    pub db_conn: Connection,
    simulate: bool,
}

impl fmt::Debug for StateMachine {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.state)
    }
}

impl StateMachine {
    fn new(disk: Device, db_conn: Connection, simulate: bool) -> Self {
        StateMachine {
            graph: GraphMap::new(),
            state: State::Unscanned,
            disk: disk,
            db_conn: db_conn,
            simulate: simulate,
        }
    }

    fn add_transition(
        &mut self,
        from_state: State,
        to_state: State,
        callback: fn(
            from_state: &State,
            to_state: &State,
            device: &Device,
            db_conn: &Connection,
            simulate: bool,
        ) -> State,
    ) {
        self.graph.add_edge(from_state, to_state, callback);
    }

    // Restore the state of this machine from the database if it was previously saved
    // otherwise do nothing and start over at Unscanned
    fn restore_state(&mut self) -> ::std::result::Result<(), rusqlite::Error> {
        let tmp = format!("/dev/{}", self.disk.name);
        let dev_path = Path::new(&tmp);
        if let Some(s) = get_state(&self.db_conn, &dev_path)? {
            self.state = s;
        }

        Ok(())
    }

    // Run all transitions until we can't go any further and return
    fn run(&mut self) {
        // Start at the current state the disk is at and work our way down the graph
        debug!("Starting state: {}", self.state);
        let tmp = format!("/dev/{}", self.disk.name);
        let dev_path = Path::new(&tmp);
        let mut done = false;
        while !done {
            let next_transition = self.graph.edges(self.state).next();
            match next_transition {
                Some(n) => {
                    // Run the transition
                    debug!("Attemption {} to {} transition", &n.0, &n.1);
                    self.state = n.2(&n.0, &n.1, &self.disk, &self.db_conn, self.simulate);
                    // Save state after every transition in case of power failure, etc
                    save_state(&self.db_conn, &dev_path, self.state).expect("save_state failed");
                    if self.state == State::WaitingForReplacement {
                        //|| self.state == State::Fail {
                        // TODO: Are these the only states we shouldn't advance further from?
                        break;
                    }
                }
                None => {
                    done = true;
                }
            };
        }
    }

    fn print_graph(&self) {
        // Walk the graph and create a Dot
        println!("digraph state_machine{{");
        for n in self.graph.nodes() {
            println!("\t{:?}[label=\"{:?}\"];", n, n);
        }
        for edge in self.graph.all_edges() {
            println!("\t{:?} -> {:?}[label=\"\"];", edge.0, edge.1);
        }
        println!("}}");
    }

    // Add all the transition states here
    fn setup_state_machine(&mut self) {
        self.add_transition(State::Unscanned, State::Scanned, Scan::transition);
        self.add_transition(State::Unscanned, State::Fail, Scan::transition);
        self.add_transition(State::NotMounted, State::Mounted, Mount::transition);
        self.add_transition(State::NotMounted, State::MountFailed, Mount::transition);
        self.add_transition(
            State::MountFailed,
            State::Corrupt,
            CheckForCorruption::transition,
        );

        self.add_transition(State::Scanned, State::Good, Eval::transition);
        self.add_transition(State::Scanned, State::NotMounted, Scan::transition);
        self.add_transition(State::Scanned, State::WriteFailed, Eval::transition);
        self.add_transition(
            State::Scanned,
            State::WornOut,
            CheckWearLeveling::transition,
        );

        self.add_transition(State::Mounted, State::Scanned, NoOp::transition);
        self.add_transition(State::ReadOnly, State::Mounted, Remount::transition);
        self.add_transition(State::ReadOnly, State::MountFailed, Remount::transition);

        self.add_transition(State::Corrupt, State::Repaired, AttemptRepair::transition);
        self.add_transition(
            State::Corrupt,
            State::RepairFailed,
            AttemptRepair::transition,
        );

        self.add_transition(
            State::RepairFailed,
            State::Reformatted,
            Reformat::transition,
        );
        self.add_transition(
            State::RepairFailed,
            State::ReformatFailed,
            Reformat::transition,
        );

        self.add_transition(
            State::ReformatFailed,
            State::WaitingForReplacement,
            NoOp::transition,
        );

        self.add_transition(State::Reformatted, State::Unscanned, NoOp::transition);

        self.add_transition(
            State::WornOut,
            State::WaitingForReplacement,
            MarkForReplacement::transition,
        );

        self.add_transition(State::Repaired, State::Good, NoOp::transition);
        self.add_transition(
            State::WaitingForReplacement,
            State::Replaced,
            Replace::transition,
        );
        self.add_transition(State::Replaced, State::Unscanned, NoOp::transition);

        self.add_transition(
            State::WriteFailed,
            State::Corrupt,
            CheckForCorruption::transition,
        );
        self.add_transition(State::WriteFailed, State::ReadOnly, Eval::transition);
        /*
        self.add_transition(
            State::Scanned,
            State::Corrupt,
            CheckForCorruption::transition,
        );
        */
        /*
        self.add_transition(
            State::Corrupt,
            State::WaitingForReplacement,
            MarkForReplacement::transition,
        );
        */
    }
}

#[derive(Debug, Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum State {
    // If the disk is in the corrupted state repairs are attempted
    Corrupt,
    Fail,
    Good,
    Mounted,
    MountFailed,
    // Should be mounted but isn't
    NotMounted,
    // Device is mounted read only
    ReadOnly,
    // Tried to reformat but failed
    ReformatFailed,
    Reformatted,
    // Tried to repair corruption and failed
    RepairFailed,
    Repaired,
    Replaced,
    Scanned,
    Unscanned,
    // The disk could not be repaired and needs to be replaced
    WaitingForReplacement,
    WornOut,
    // Write test failed
    WriteFailed,
}

impl FromStr for State {
    type Err = String;

    fn from_str(s: &str) -> ::std::result::Result<Self, Self::Err> {
        match s {
            "corrupt" => Ok(State::Corrupt),
            "fail" => Ok(State::Fail),
            "good" => Ok(State::Good),
            "mounted" => Ok(State::Mounted),
            "mount_failed" => Ok(State::MountFailed),
            "readonly" => Ok(State::ReadOnly),
            "reformatted" => Ok(State::Reformatted),
            "reformat_failed" => Ok(State::ReformatFailed),
            "repaired" => Ok(State::Repaired),
            "repair_failed" => Ok(State::RepairFailed),
            "replaced" => Ok(State::Replaced),
            "scanned" => Ok(State::Scanned),
            "unscanned" => Ok(State::Unscanned),
            "waiting_for_replacement" => Ok(State::WaitingForReplacement),
            "worn_out" => Ok(State::WornOut),
            _ => Err(format!("Unknown state: {}", s)),
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            State::Corrupt => write!(f, "corrupt"),
            State::Fail => write!(f, "fail"),
            State::Good => write!(f, "good"),
            State::Mounted => write!(f, "mounted"),
            State::MountFailed => write!(f, "mount_failed"),
            State::NotMounted => write!(f, "not_mounted"),
            State::ReadOnly => write!(f, "readonly"),
            State::RepairFailed => write!(f, "repair_failed"),
            State::ReformatFailed => write!(f, "reformat_failed"),
            State::Reformatted => write!(f, "reformatted"),
            State::Repaired => write!(f, "repaired"),
            State::Replaced => write!(f, "replaced"),
            State::Scanned => write!(f, "scanned"),
            State::Unscanned => write!(f, "unscanned"),
            State::WaitingForReplacement => write!(f, "waiting_for_replacement"),
            State::WriteFailed => write!(f, "write_failed"),
            State::WornOut => write!(f, "worn_out"),
        }
    }
}

// Transitions
#[derive(Debug)]
struct AttemptRepair;

#[derive(Debug)]
struct CheckForCorruption;

#[derive(Debug)]
struct CheckWearLeveling;

#[derive(Debug)]
struct Eval;

#[derive(Debug)]
struct MarkForReplacement;

#[derive(Debug)]
struct Mount;

#[derive(Debug)]
struct NoOp;

#[derive(Debug)]
struct Remount;

#[derive(Debug)]
struct Replace;

#[derive(Debug)]
struct Reformat;

#[derive(Debug)]
struct Scan;
// Transitions

pub fn check_all_disks(db: &Path) -> Result<Vec<Result<StateMachine>>> {
    let results: Vec<Result<StateMachine>> = Vec::new();
    // Udev will only show the disks that are currently attached to the tree
    // It will fail to show disks that have died and disconnected but are still
    // shown as mounted in /etc/mtab
    let devices = block_utils::get_block_devices().map_err(|e| Error::new(ErrorKind::Other, e))?;

    // Gather info on all devices and skip Loopback devices
    let device_info: Vec<Device> = block_utils::get_all_device_info(devices.as_slice())
        .map_err(|e| Error::new(ErrorKind::Other, e))?
        .into_iter()
        // Get rid of loopback devices
        .filter(|d| !(d.media_type == MediaType::Loopback))
        // Get rid of lvm devices
        .filter(|d| !(d.media_type == MediaType::LVM))
        // Get rid of ram devices
        .filter(|d| !(d.media_type == MediaType::Ram))
        .collect();

    // Gather info on all the currently mounted devices
    let mut mtab_devices: Vec<Device> = block_utils::get_mounted_devices()?;

    // Remove any mtab_devices that udev already knows about leaving only ones
    // that udev doesn't know about, ie broken mounted devices
    mtab_devices.retain(|mtab_device| {
        !device_info
            .iter()
            .any(|udev_device| mtab_device.name.contains(&udev_device.name))
    });

    // Check any devices that udev doesn't know about that are still mounted
    for mtab_device in mtab_devices {
        //results.push(run_checks(&mtab_device));
    }

    //TODO: Add nvme devices to block-utils

    // Create 1 state machine per Device and evaulate all devices in parallel
    let disk_states: Vec<rusqlite::Result<StateMachine>> = device_info
        .clone()
        .into_par_iter()
        .map(|device| {
            // Lookup the disk and see if it's in progress.  If so then
            // set the state to WaitingOnReplacement.
            // Resume where we left off
            let conn = connect_to_repair_database(db)?;
            let mut s = StateMachine::new(device, conn, false);
            s.setup_state_machine();
            s.restore_state()?;
            s.run();
            // Possibly serialize the state here to the database to resume later
            if s.state == State::WaitingForReplacement {
                info!("Connecting to database to check if disk is in progress");
                let tmp = format!("/dev/{}", s.disk.name);
                let disk_path = Path::new(&tmp);
                let conn = connect_to_repair_database(db)?;
                let in_progress = is_disk_in_progress(&conn, &disk_path)?;
            }
            Ok(s)
        }).collect();

    Ok(results)
}

/*
fn run_checks(device_info: &Device) -> Result<Status> {
    let mut disk_status = Status {
        corrupted: false,
        repaired: false,
        device: device_info.clone(),
        mount_path: PathBuf::from(""),
        smart_passed: None,
    };
    let dev_path = format!("/dev/{}", device_info.name);

    // Run a smart check on the base device without partition
    match run_smart_checks(&Path::new(&dev_path)) {
        Ok(result) => {
            disk_status.smart_passed = Some(result);
        }
        Err(e) => {
            error!("Smart test failed: {:?}", e);
        }
    };

    let device = Path::new(&dev_path);
    match get_mountpoint(&device) {
        Ok(mount_info) => {
            match mount_info {
                Some(s) => {
                    // mounted at s
                    info!("Device is mounted at: {:?}", s);
                    debug!("Checking if device exists: {:?}", device);
                    match device.exists() {
                        true => {
                            debug!("udev Probing device {:?}", device);
                            let info = block_utils::get_device_info(&device);
                            let corrupted = match check_writable(&s) {
                                Ok(_) => false,
                                Err(e) => {
                                    //Should proceed to error checking now
                                    error!("Error writing to disk: {:?}", e);
                                    disk_status.corrupted = true;
                                    true
                                }
                            };
                            if corrupted {
                                if let Ok(udev_info) = info {
                                    let check_result =
                                        check_filesystem(&udev_info.fs_type, &device);
                                    debug!("check_filesystem result: {:?}", check_result);
                                    let repair_result =
                                        repair_filesystem(&udev_info.fs_type, &device);
                                    debug!("repair_result result: {:?}", repair_result);
                                } else {
                                    error!(
                                        "Failed to gather udev info on {:?}. error: {:?}",
                                        device, info
                                    );
                                }
                            }
                        }
                        false => {
                            // mountpoint exists for device that does not exist.  Lets flag it
                            // so it gets checked out by a human
                            debug!(
                                "Device does not exist: {:?} but system thinks it is mounted",
                                device
                            );
                            disk_status.corrupted = true;
                        }
                    };
                }
                None => {
                    // It's not mounted.  Lets run an check/repair on it
                    debug!("Device is not mounted: {:?}", device);
                }
            };
        }
        Err(e) => {
            error!("Failed to determine if device is mounted.  {:?}", e);
        }
    };
    Ok(disk_status)
}
*/

fn check_filesystem(filesystem_type: &FilesystemType, device: &Path) -> Result<()> {
    match filesystem_type {
        &FilesystemType::Ext2 => Ok(check_ext(device)?),
        &FilesystemType::Ext3 => Ok(check_ext(device)?),
        &FilesystemType::Ext4 => Ok(check_ext(device)?),
        &FilesystemType::Xfs => Ok(check_xfs(device)?),
        _ => Err(Error::new(ErrorKind::Other, "Unknown filesystem detected")),
    }
}

fn repair_filesystem(filesystem_type: &FilesystemType, device: &Path) -> Result<()> {
    match filesystem_type {
        &FilesystemType::Ext2 => Ok(repair_ext(device)?),
        &FilesystemType::Ext3 => Ok(repair_ext(device)?),
        &FilesystemType::Ext4 => Ok(repair_ext(device)?),
        &FilesystemType::Xfs => Ok(repair_xfs(device)?),
        _ => Err(Error::new(ErrorKind::Other, "Unknown filesystem detected")),
    }
}

fn check_writable(path: &Path) -> Result<()> {
    debug!("Checking if {:?} is writable", path);
    let temp_path = TempDir::new_in(path, "bynar")?;
    let file_path = temp_path.path().join("write_test");
    debug!("Creating: {}", file_path.display());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(file_path)?;
    file.write_all(b"Hello, world!")?;
    Ok(())
}

fn check_xfs(device: &Path) -> Result<()> {
    //Any output that is produced when xfs_check is not run in verbose mode
    //indicates that the filesystem has an inconsistency.
    debug!("Running xfs_repair -n to check for corruption");
    let status = Command::new("xfs_repair")
        .args(&vec!["-n", &device.to_string_lossy()])
        .status()?;
    match status.code() {
        Some(code) => match code {
            0 => return Ok(()),
            1 => {
                return Err(Error::new(
                    ErrorKind::Other,
                    "Filesystem corruption detected",
                ))
            }
            _ => {}
        },
        //Process terminated by signal
        None => {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "xfs_repair terminated by signal",
            ))
        }
    }
    Ok(())
}

fn repair_xfs(device: &Path) -> Result<()> {
    debug!("Running xfs_repair");
    let status = Command::new("xfs_repair").arg(device).status()?;
    match status.code() {
        Some(code) => match code {
            0 => return Ok(()),
            _ => return Err(Error::new(ErrorKind::Other, "xfs_repair failed")),
        },
        //Process terminated by signal
        None => {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "e2fsck terminated by signal",
            ))
        }
    };
}

fn check_ext(device: &Path) -> Result<()> {
    debug!("running e2fsck -n to check for errors");
    let status = Command::new("e2fsck")
        .args(&["-n", &device.to_string_lossy()])
        .status()?;
    match status.code() {
        Some(code) => {
            match code {
                //0 - No errors
                0 => return Ok(()),
                //4 - File system errors left uncorrected.  This requires repair
                4 => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("e2fsck returned error code: {}", code),
                    ))
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("e2fsck returned error code: {}", code),
                    ))
                }
            }
        }
        //Process terminated by signal
        None => {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "e2fsck terminated by signal",
            ))
        }
    }
}

fn repair_ext(device: &Path) -> Result<()> {
    //Run a noninteractive fix.  This will exit with return code 4
    //if it needs human intervention.
    debug!("running e2fsck -p for noninteractive repair");
    let status = Command::new("e2fsck")
        .args(&["-p", &device.to_string_lossy()])
        .status()?;
    match status.code() {
        Some(code) => {
            match code {
                //0 - No errors
                0 => return Ok(()),
                // 1 - File system errors corrected
                1 => return Ok(()),
                //2 - File system errors corrected, system should
                //be rebooted
                2 => return Ok(()),
                _ => {
                    return Err(Error::new(
                        ErrorKind::Other,
                        format!("e2fsck returned error code: {}", code),
                    ))
                }
            }
        }
        //Process terminated by signal
        None => {
            return Err(Error::new(
                ErrorKind::Interrupted,
                "e2fsck terminated by signal",
            ))
        }
    }
}

// Run smart checks against the disk
#[cfg_attr(test, mockable)]
fn run_smart_checks(device: &Path) -> Result<bool> {
    let mut smart = libatasmart::Disk::new(device).map_err(|e| Error::new(ErrorKind::Other, e))?;
    let status = smart
        .get_smart_status()
        .map_err(|e| Error::new(ErrorKind::Other, e))?;
    Ok(status)
}
