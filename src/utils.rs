//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright © 2025 Areg Baghinyan. All Rights Reserved.
//
// Author(s): Areg Baghinyan
//
use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce}; // AES-GCM cipher
use anyhow::{Error, Result};
use chrono::{DateTime, Local};
use filetime::{set_file_handle_times, FileTime};
use ntfs::{NtfsAttribute, NtfsAttributeType, NtfsFile, NtfsReadSeek};
use rand::RngCore;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::fs::File;
use std::fs::{create_dir_all, OpenOptions};
use std::io;
use std::io::ErrorKind;
use std::io::SeekFrom;
use std::io::{Read, Seek, Write};
use std::path::Path;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub fn get<T>(
    file: &NtfsFile,
    file_name: &str,
    out_dir: &str,
    fs: &mut T,
    encrypt: Option<&String>,
    ads: &str,
    drive: &str,
) -> Result<bool, Error>
where
    T: Read + Seek,
{
    // Check if encryption is required and construct the output file name
    let mut output_file_name = if let Some(ref password) = encrypt {
        if !password.is_empty() {
            let path = Path::new(&file_name);
            let new_file_name = if let Some(extension) = path.extension() {
                format!("{}.enc", path.with_extension(extension).to_string_lossy())
            } else {
                format!("{}.enc", path.to_string_lossy())
            };
            format!("{}{}", out_dir, new_file_name)
        } else {
            format!("{}{}", out_dir, file_name)
        }
    } else {
        format!("{}{}", out_dir, file_name)
    };

    // Try to create the directory, log error if it fails
    if let Err(e) = create_dir_all(
        output_file_name
            .rfind('/')
            .map(|pos| &output_file_name[..pos])
            .unwrap_or(""),
    ) {
        return Err(anyhow::anyhow!(
            "[ERROR] Failed to create directory `{}`: {}",
            out_dir,
            e
        ));
    }
    let is_ads = !(ads.is_empty() || ads == "");

    // Append the Alternate Data Stream (ADS) name if it's not empty
    output_file_name = output_file_name.replace(":", "%3A");

    // Try to open the file for writing, log error if it fails
    let mut output_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_file_name)
    {
        Ok(f) => f,
        Err(ref e) if e.kind() == ErrorKind::AlreadyExists => {
            return Ok(false);
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "[ERROR] Failed to open file `{}` for writing: {}",
                output_file_name,
                e
            ));
        }
    };
    if !is_ads {
        // Iterate over attributes to find $INDEX_ALLOCATION
        let attributes: Vec<_> = file
            .attributes()
            .attach(fs)
            .collect::<Result<Vec<_>, _>>()?;
        for attribute in attributes {
            match attribute.to_attribute() {
                Ok(attr) => {
                    if attr.ty()? == NtfsAttributeType::IndexAllocation {
                        get_attr(&attr, fs, &output_file_name)?;
                    }
                }
                Err(_) => dprintln!("[ERROR] Can't getting attributes"),
            }
        }
    }

    // Try to get the data item, log warning if it does not exist
    let data_item = match file.data(fs, ads) {
        Some(Ok(item)) => item,
        Some(Err(e)) => {
            return Err(anyhow::anyhow!(
                "[ERROR] Failed to retrieve data for `{}`: {}",
                file_name,
                e
            ));
        }
        None => {
            return Err(anyhow::anyhow!(
                "[WARN] The file {} does not have a $DATA attribute.",
                output_file_name
            ));
        }
    };
    let data_attribute = match data_item.to_attribute() {
        Ok(attr) => attr,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "[ERROR] Failed to retrieve attribute for `{}`: {}",
                file_name,
                e
            ));
        }
    };

    let mut data_value = match data_attribute.value(fs) {
        Ok(val) => val,
        Err(e) => {
            return Err(anyhow::anyhow!(
                "[ERROR] Failed to retrieve data value for `{}`: {}",
                file_name,
                e
            ));
        }
    };

    // Get the valid data length
    let valid_data_length = get_valid_data_length(fs, &data_attribute)?;

    dprintln!(
        "[INFO] Saving {} bytes of data in `{}`",
        &valid_data_length,
        output_file_name
    );

    // Buffer for reading chunks of the file
    let mut read_buf = [0u8; 4096];

    // Stream data based on encryption
    if let Some(ref password) = encrypt {
        if !password.is_empty() {
            // Derive the encryption key using SHA256
            let mut hasher = Sha256::new();
            hasher.update(password.as_bytes());
            let key_bytes = hasher.finalize();
            let cipher_key = Key::<Aes256Gcm>::from_slice(&key_bytes[..32]); // AES-256 requires a 32-byte key
            let cipher = Aes256Gcm::new(cipher_key);

            // Generate a nonce (unique for each message)
            let mut nonce = [0u8; 12]; // 96-bit nonce for AES-GCM
            OsRng.fill_bytes(&mut nonce);
            let nonce = Nonce::from_slice(&nonce);

            // Write the nonce to the file before writing encrypted data
            if output_file.write_all(nonce).is_err() {
                return Err(anyhow::anyhow!(
                    "[ERROR] Failed to write nonce to `{}`",
                    output_file_name
                ));
            }
            let mut current_file_size: u64 = 0;
            // Stream data, encrypt each chunk, and write it to the file
            loop {
                match data_value.read(fs, &mut read_buf) {
                    Ok(bytes_read) => {
                        if bytes_read == 0 {
                            break;
                        }
                        if !data_attribute.is_resident() && !is_ads {
                            current_file_size += bytes_read as u64;
                            if current_file_size > valid_data_length {
                                // Write remaining data (including current read buffer) to a "slack" file
                                let mut slack_file = match OpenOptions::new()
                                    .write(true)
                                    .create_new(true)
                                    .open(&format!("{}.FileSlack", output_file_name))
                                {
                                    Ok(f) => f,
                                    Err(ref e) if e.kind() == ErrorKind::AlreadyExists => {
                                        return Ok(false);
                                    }
                                    Err(e) => {
                                        return Err(anyhow::anyhow!(
                                            "[ERROR] Failed to open file `{}` for writing: {}",
                                            format!("{}.FileSlack", output_file_name),
                                            e
                                        ));
                                    }
                                };

                                // Write the remaining part of the current buffer to the slack file
                                let start_slack =
                                    (valid_data_length - (current_file_size - bytes_read as u64)) as usize;
                                slack_file.write_all(&read_buf[start_slack..bytes_read])?;

                                // padding with 0
                                let mut padding = vec![0; bytes_read - start_slack];
                                output_file.write_all(&padding)?;

                                // Continue reading and writing all remaining data to the slack file
                                while let Ok(slack_bytes_read) = data_value.read(fs, &mut read_buf) {
                                    if slack_bytes_read == 0 {
                                        break;
                                    }
                                    slack_file.write_all(&read_buf[..slack_bytes_read])?;

                                    padding = vec![0; slack_bytes_read];
                                    output_file.write_all(&padding)?;
                                }
                                break;
                            }
                        }

                        let chunk = if is_ads && read_buf.iter().all(|&b| b == 0) {
                            continue;
                        } else {
                            &read_buf[..bytes_read]
                        };

                        let encrypted_chunk = match cipher.encrypt(nonce, chunk) {
                            Ok(ct) => ct,
                            Err(e) => {
                                return Err(anyhow::anyhow!("[ERROR] Encryption failed: {}", e));
                            }
                        };

                        // Write the encrypted chunk to the output file
                        if output_file.write_all(&encrypted_chunk).is_err() {
                            return Err(anyhow::anyhow!(
                                "[ERROR] Failed to write encrypted chunk to `{}`",
                                output_file_name
                            ));
                        }
                    },
                    Err(err) => {
                        dprintln!("[ERROR] Reading data: {:?}", err);
                        break
                    }
                }
            }
        }
    } else {
        // No encryption, write the file normally in chunks
        if file_name == "/$Boot" {
            output_file.write_all(&get_boot(&drive).unwrap()).unwrap();
        } else {
            let mut current_file_size: u64 = 0;
            loop {
                match data_value.read(fs, &mut read_buf) {
                    Ok(bytes_read) => {
                        if bytes_read == 0 {
                            break;
                        }
                        if !data_attribute.is_resident() && !is_ads {
                            current_file_size += bytes_read as u64;
                            // Check if the Valid data is reached
                            if current_file_size > valid_data_length {
                                // Write remaining data (including current read buffer) to a "slack" file
                                let mut slack_file = match OpenOptions::new()
                                    .write(true)
                                    .create_new(true)
                                    .open(&format!("{}.FileSlack", output_file_name))
                                {
                                    Ok(f) => f,
                                    Err(ref e) if e.kind() == ErrorKind::AlreadyExists => {
                                        return Ok(false);
                                    }
                                    Err(e) => {
                                        return Err(anyhow::anyhow!(
                                            "[ERROR] Failed to open file `{}` for writing: {}",
                                            format!("{}.FileSlack", output_file_name),
                                            e
                                        ));
                                    }
                                };

                                // Write the remaining part of the current buffer to the slack file
                                let start_slack =
                                    (valid_data_length - (current_file_size - bytes_read as u64)) as usize;
                                slack_file.write_all(&read_buf[start_slack..bytes_read])?;

                                // padding with 0
                                let mut padding = vec![0; bytes_read - start_slack];
                                output_file.write_all(&padding)?;

                                // Continue reading and writing all remaining data to the slack file
                                while let Ok(slack_bytes_read) = data_value.read(fs, &mut read_buf) {
                                    if slack_bytes_read == 0 {
                                        break;
                                    }
                                    slack_file.write_all(&read_buf[..slack_bytes_read])?;

                                    padding = vec![0; slack_bytes_read];
                                    output_file.write_all(&padding)?;
                                }
                                break;
                            }
                        }
                        let chunk = if is_ads && read_buf.iter().all(|&b| b == 0) {
                            continue;
                        } else {
                            &read_buf[..bytes_read]
                        };
                        if output_file.write_all(chunk).is_err() {
                            return Err(anyhow::anyhow!(
                                "[ERROR] Failed to write chunk to `{}`",
                                output_file_name
                            ));
                        }
                    },
                    Err(err) => {
                        dprintln!("[ERROR] Reading data: {:?}", err);
                        break
                    }
                }
            }
        }
    }
    // Retrieve timestamps from NtfsFile (replace these method calls with the actual methods from NtfsFile)
    if let Ok(file_std_info) = file.info() {
        let modified_time: DateTime<Local> =
            nt_timestamp_to_datetime(file_std_info.modification_time().nt_timestamp());

        let modified_file_time = FileTime::from_system_time(add_timezone_offset_to_system_time(
            modified_time.into(),
            modified_time.offset().local_minus_utc().into(),
        ));

        set_file_handle_times(&output_file, None, Some(modified_file_time))
            .map_err(|e| anyhow::anyhow!("[ERROR] Failed to set file timestamps: {}", e))?;
    }
    match output_file.flush() {
        Ok(_) => {
            dprintln!("[INFO] Data successfully saved to `{}`", output_file_name);
            return Ok(true);
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "[ERROR] Problem to save `{}` file: {:?}",
                output_file_name,
                e
            ))
        }
    };
}

fn get_valid_data_length<T>(fs: &mut T, attribut: &NtfsAttribute) -> Result<u64, Error>
where
    T: Read + Seek,
{
    return match &attribut.ty()? {
        NtfsAttributeType::Data => match attribut.position().value() {
            Some(data_attr_position) => {
                let mut buff = vec![0u8; 64];
                fs.seek(SeekFrom::Start(data_attr_position.get()))?;
                fs.read_exact(&mut buff)?;
                let byte_57 = buff[56];
                let byte_58 = buff[57];
                let byte_59 = buff[58];
                let byte_60 = buff[59];
                let vdl = ((byte_60 as u64) << 24)
                    | ((byte_59 as u64) << 16)
                    | ((byte_58 as u64) << 8)
                    | (byte_57 as u64);
                Ok(vdl)
            }
            None => Err(anyhow::anyhow!("[ERROR] $DATA position not found")),
        },
        _ => Err(anyhow::anyhow!("[ERROR] Wrong attribut type")),
    };
}

fn get_attr<T>(attr: &NtfsAttribute, fs: &mut T, output_file_name: &str) -> Result<(), Error>
where
    T: Read + Seek,
{
    let attr_name = attr.name()?.to_string_lossy().to_string();
    dprintln!(
        "[INFO] Found $INDEX_ALLOCATION attribute : `{}`",
        &attr_name
    );

    let attr_path = format!("{}%3A{}.idx", output_file_name, &attr_name);
    let mut attr_value = attr.value(fs)?;

    let mut output_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&attr_path)
    {
        Ok(f) => f,
        Err(ref e) if e.kind() == ErrorKind::AlreadyExists => {
            return Ok(());
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "[ERROR] Failed to open file `{}` for writing: {}",
                &attr_path,
                e
            ));
        }
    };
    dprintln!(
        "[INFO] Saving {} bytes of index attribute data in `{}`",
        attr_value.len(),
        &attr_path
    );
    let mut read_buf = [0u8; 4096];

    while let Ok(bytes_read) = attr_value.read(fs, &mut read_buf) {
        if bytes_read == 0 {
            // End of file reached
            break;
        }
        if output_file.write_all(&read_buf[..bytes_read]).is_err() {
            return Err(anyhow::anyhow!(
                "[ERROR] Failed to write chunk to `{}`",
                attr_path
            ));
        }
    }

    Ok(())
}

// Function to convert NT timestamp (u64) to SystemTime
fn nt_timestamp_to_system_time(nt_timestamp: u64) -> SystemTime {
    // NT Epoch: January 1, 1601 -> UNIX Epoch: January 1, 1970 (difference in seconds)
    let nt_epoch_to_unix_epoch = Duration::from_secs(11644473600); // 369 years in seconds
    let timestamp_duration = Duration::from_nanos(nt_timestamp * 100); // Convert to nanoseconds
    UNIX_EPOCH + timestamp_duration - nt_epoch_to_unix_epoch
}

fn nt_timestamp_to_datetime(nt_timestamp: u64) -> DateTime<Local> {
    let system_time = nt_timestamp_to_system_time(nt_timestamp);
    DateTime::<Local>::from(system_time)
}

fn add_timezone_offset_to_system_time(system_time: SystemTime, offset_seconds: i64) -> SystemTime {
    if offset_seconds >= 0 {
        system_time + Duration::from_secs(offset_seconds as u64)
    } else {
        system_time - Duration::from_secs((-offset_seconds) as u64)
    }
}

fn get_boot(drive_letter: &str) -> Result<Vec<u8>, Error> {
    let drive_path = format!("\\\\.\\{}:", drive_letter); // Raw access to the drive

    // Check if the drive exists before attempting to open it
    if Path::new(&format!("{}:\\", drive_letter)).exists() {
        let mut file = File::open(&drive_path).unwrap();
        let mut boot_sector = vec![0u8; 8192];

        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut boot_sector)?;

        return Ok(boot_sector);
    }

    Err(anyhow::anyhow!(
        "[ERROR] Drive {} does not exist",
        drive_letter
    ))
}

pub fn ensure_directory_exists(path: &str) -> std::io::Result<()> {
    let path = Path::new(path);
    if !path.exists() {
        fs::create_dir_all(path)?;
        dprintln!("[INFO] Directory {} is created", path.display());
    }
    Ok(())
}

pub fn replace_env_vars(input: &str) -> String {
    // Regex pattern to match %VAR_NAME% or %SYSTEM_VAR_NAME%
    let re = Regex::new(r"%([^%]+)%").unwrap();

    // Replace each match with the corresponding environment variable value
    let result = re.replace_all(input, |caps: &regex::Captures| {
        let var_name = &caps[1];
        env::var(var_name).unwrap_or_else(|_| format!("%{}%", var_name))
    });

    let replaced_str = result.into_owned(); // Convert to owned String
    let regex = Regex::new(r"^[A-Za-z]:\\").unwrap(); // Match a single letter at the start followed by :\
    let replaced_str = regex.replace(&replaced_str, r"\");

    replaced_str.to_string()
}

pub fn remove_dir_all(path: &str) -> io::Result<()> {
    let path = Path::new(path); // Convert the string to a Path
    if path.is_dir() {
        // Iterate over all entries in the directory
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let entry_path = entry.path();

            // Recursively remove directory contents or remove the file
            if entry_path.is_dir() {
                // Convert Path to &str safely and recursively call remove_dir_all
                if let Some(entry_str) = entry_path.to_str() {
                    remove_dir_all(entry_str)?; // Recursively call the function and propagate errors
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Invalid UTF-8 sequence in path",
                    ));
                }
            } else {
                // If the entry is a file, remove it
                fs::remove_file(&entry_path)?;
            }
        }
        // Once the directory is empty, remove the directory itself
        fs::remove_dir(path)?;
    }
    Ok(())
}

pub fn remove_trailing_slash(input: String) -> String {
    input.strip_suffix('/').unwrap_or(&input).to_string()
}

pub fn split_path(input: &str) -> (String, String) {
    if let Some((path, last_segment)) = input.rsplit_once('/') {
        (path.to_string(), last_segment.to_string())
    } else {
        (String::new(), input.to_string())
    }
}
