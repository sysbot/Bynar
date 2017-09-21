
/// Detect dead disks in a ceph cluster
/// 1. Detect dead disk
/// 2. Report dead disk to JIRA for repairs
/// 3. Test for resolution
/// 4. Put disk back into cluster
#[macro_use]
extern crate clap;
#[macro_use]
extern crate serde_derive;
extern crate serde;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate log;
extern crate simplelog;

mod backend;
mod create_support_ticket;
mod host_information;
mod in_progress;
mod test_disk;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use create_support_ticket::{create_support_ticket, ticket_resolved};
use clap::{Arg, App};
use host_information::Host;
use simplelog::{Config, SimpleLogger};

#[derive(Clone, Debug, Deserialize)]
pub struct ConfigSettings {
    backend: backend::BackendType,
    db_location: String,
    pub jira_user: String,
    pub jira_password: String,
    pub jira_host: String,
    pub jira_issue_type: String,
    pub jira_priority: String,
    pub jira_project_id: String,
    pub jira_ticket_assignee: String,
    pub proxy: Option<String>,
}

fn load_config(config_dir: &str) -> Result<ConfigSettings, String> {
    let c: ConfigSettings = {
        let mut f = File::open(format!("{}/config.json", config_dir)).map_err(
            |e| {
                e.to_string()
            },
        )?;
        let mut s = String::new();
        f.read_to_string(&mut s).map_err(|e| e.to_string())?;
        let deserialized: ConfigSettings = serde_json::from_str(&s).map_err(|e| e.to_string())?;
        deserialized
    };
    Ok(c)
}

fn check_for_failed_disks(config_dir: &str, simulate: bool) -> Result<(), String> {
    let config = load_config(config_dir)?;
    let config_location = Path::new(&config.db_location);
    //Host information to use in ticket creation
    let host_info = Host::new().map_err(|e| e.to_string())?;
    debug!("Gathered host info: {:?}", host_info);
    let mut description = format!("A disk on {} failed. Please replace.", host_info.hostname);
    let mut environment =
        format!(
        "Hostname: {}\nServer type: {}\nServer Serial: {}\nMachine Architecture: {}\nKernel: {}",
        host_info.hostname,
        host_info.server_type,
        host_info.serial_number,
        host_info.machine_architecture,
        host_info.kernel,
    );

    debug!("Loading ceph backend");
    let backend = backend::load_backend(&backend::BackendType::Ceph, Some(Path::new(&config_dir)))?;
    info!("Checking all drives");
    for result in test_disk::check_all_disks().map_err(|e| e.to_string())? {
        match result {
            Ok(status) => {
                info!("Disk status: {:?}", status);
                let mut dev_path = PathBuf::from("/dev");
                dev_path.push(status.device.name);

                if status.corrupted == true && status.repaired == false {
                    description.push_str(&format!("\nDisk path: {}", dev_path.display()));
                    if let Some(serial) = status.device.serial_number {
                        description.push_str(&format!("\nDisk serial: {}", serial));
                    }
                    info!("Connecting to database to check if disk is in progress");
                    let conn = in_progress::create_repair_database(&config_location)
                        .map_err(|e| e.to_string())?;
                    let in_progress = in_progress::is_disk_in_progress(&conn, &dev_path).map_err(
                        |e| {
                            e.to_string()
                        },
                    )?;
                    if !simulate {
                        if !in_progress {
                            debug!("Asking backend to remove disk");
                            let _ = backend.remove_disk(&dev_path, simulate).map_err(
                                |e| e.to_string(),
                            )?;
                            debug!("Creating support ticket");
                            let ticket_id = create_support_ticket(
                                &config,
                                "Dead disk",
                                &description,
                                &environment,
                            ).map_err(|e| format!("{:?}", e))?;
                            debug!("Recording ticket id {} in database", ticket_id);
                            in_progress::record_new_repair_ticket(&conn, &ticket_id, &dev_path)
                                .map_err(|e| e.to_string())?;
                        } else {
                            debug!("Device is already in the repair queue");
                        }
                    }
                }
            }
            Err(e) => {
                //
                error!("check_all_disks failed with error: {:?}", e);
                return Err(format!("check_all_disks failed with error: {:?}", e));
            }
        };
    }
    Ok(())
}

fn add_repaired_disks(config_dir: &str, simulate: bool) -> Result<(), String> {
    let config = load_config(config_dir)?;
    let config_location = Path::new(&config.db_location);

    info!("Connecting to database to find repaired drives");
    let conn = in_progress::create_repair_database(&config_location)
        .map_err(|e| e.to_string())?;
    debug!("Loading Ceph backend");
    let backend =
        backend::load_backend(&backend::BackendType::Ceph, Some(&Path::new(&config_dir)))?;
    info!("Getting outstanding repair tickets");
    let tickets = in_progress::get_outstanding_repair_tickets(&conn).map_err(
        |e| {
            e.to_string()
        },
    )?;
    info!("Checking for resolved repair tickets");
    for ticket in tickets {
        let resolved = ticket_resolved(&config, &ticket.id.to_string()).map_err(
            |e| {
                e.to_string()
            },
        )?;
        if resolved {
            let _ = backend
                .add_disk(&Path::new(&ticket.disk_path), simulate)
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

// 1. Gather a list of all the disks
// 2. Check every disk
// 3. Decide if a disk needs to be replaced
// 4. File a ticket
// 5. Record the replacement in the in_progress sqlite database

fn main() {
    let matches = App::new("Ceph Disk Manager")
        .version(crate_version!())
        .author(crate_authors!())
        .about(
            "Detect dead hard drives, create a support ticket and watch for resolution",
        )
        .arg(
            Arg::with_name("configdir")
                .default_value("/etc/ceph_dead_disk")
                .help("The directory where all config files can be found")
                .long("configdir")
                .takes_value(true)
                .required(false),
        )
        .arg(
            Arg::with_name("simulate")
                .help("Log messages but take no action")
                .long("simulate")
                .required(false),
        )
        .arg(Arg::with_name("v").short("v").multiple(true).help(
            "Sets the level of verbosity",
        ))
        .get_matches();
    let level = match matches.occurrences_of("v") {
        0 => log::LogLevelFilter::Info, //default
        1 => log::LogLevelFilter::Debug,
        _ => log::LogLevelFilter::Trace,
    };
    let _ = SimpleLogger::init(level, Config::default());
    info!("Starting up");

    //Sanity check
    if !Path::new("/etc/ceph_dead_disk").exists() {
        error!("Config directory doesn't exist. Please create it or use the --configdir option");
        return;
    }
    let simulate = matches.is_present("simulate");

    let config_dir = matches.value_of("configdir").unwrap();

    match check_for_failed_disks(config_dir, simulate) {
        Err(e) => {
            error!("Check for failed disks failed with error: {}", e);
        }
        _ => {
            info!("Check for failed disks completed");
        }
    };
    match add_repaired_disks(config_dir, simulate) {
        Err(e) => {
            error!("Add repaired disks failed with error: {}", e);
        }
        _ => {
            info!("Add repaired disks completed");
        }
    };
}
