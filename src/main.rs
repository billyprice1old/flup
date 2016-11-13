#![feature(custom_derive, plugin, proc_macro)]
#![plugin(tojson_macros)]

#[macro_use] extern crate guard;
extern crate rustc_serialize;
#[macro_use] extern crate tojson_macros;
extern crate toml;
extern crate crypto;

use rustc_serialize::Decodable;

use std::io::{Read, Write};
use std::fs::File;
use std::path::Path;

use crypto::sha1::Sha1;
use crypto::digest::Digest;

mod db;
mod handler;

use db::FlupDb;
use handler::FlupHandler;

static ID_CHARS: &'static [char] = &['A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];

fn hash_ip(salt: String, ip: String) -> String {
    let mut hasher = Sha1::new();
    hasher.input(ip.as_bytes());
    hasher.input(salt.as_bytes());

    hasher.result_str()
}

fn hash_file_data(file_data: &[u8]) -> String {
    let mut hasher = Sha1::new();
    hasher.input(file_data);

    hasher.result_str()
}

fn handle_filename(filename: String, remove_filename: bool) -> String {
    let path = Path::new(filename.as_str());

    let short_base = match remove_filename {
        true => "file".to_string(),
        false => {
            let base_str = path.file_stem().unwrap().to_str().unwrap();
            base_str.chars().take(45).collect::<String>()
        },
    };

    match path.extension() {
        Some(ext) => {
            let ext_str = ext.to_str().unwrap();
            let short_ext: String = ext_str.chars().take(10).collect();

            format!("{}.{}", short_base, short_ext)
        },
        None => short_base,
    }
}

fn blocked_extension(blocked: &[String], filename: &String) -> bool {
    if let Some(extension) = Path::new(filename.as_str()).extension() {
        let extension_string = extension.to_str().unwrap().to_string();

        blocked.contains(&extension_string)
    } else {
        false
    }
}

fn handle_xforwarded(ips_string: String, i: usize) -> Option<String> {
    let mut ips: Vec<&str> = ips_string.split(", ").collect();
    ips.reverse();

    ips.get(i).map(|ip| ip.to_string())
}

#[derive(Debug, Clone, RustcDecodable)]
pub struct FlupConfig {
    pub host: String,
    pub url: String,

    pub salt: String,

    pub redis_prefix: String,

    pub no_access_extensions: Vec<String>,
    pub no_upload_extensions: Vec<String>,

    pub xforwarded: bool,
    pub xforwarded_index: usize,
}

#[derive(Debug, Clone, ToJson, RustcEncodable, RustcDecodable)]
pub struct FileInfo {
    pub name: String,
    pub desc: String,

    pub file_id: String,

    pub hash: String,
    pub size: u64,

    pub uploader: String,
}

#[derive(Debug, Clone, ToJson, RustcEncodable, RustcDecodable)]
pub struct DeletedFile {
    pub file: FileInfo,
    pub reason: String,
}

#[derive(Clone)]
pub struct Flup {
    config: FlupConfig,
    db: FlupDb,
}

#[derive(Debug)]
pub enum StartError {
    Db(db::StartError),
}

pub struct UploadRequestParams {
    pub files: Vec<(Option<File>, Option<String>)>,

    pub is_private: bool,
    pub no_filename: bool,
    pub desc: Option<String>,
}

pub struct UploadRequest {
    pub xforwarded: Option<String>,
    pub params: Option<UploadRequestParams>,
    pub ip: String,
}

pub struct IdGetRequest {
    pub file_id: String,
}

pub struct GetRequest {
    pub file_id: String,
    pub filename: String,
}

#[derive(Debug)]
pub enum UploadError {
    SetIp,
    NoPostParams,
    InvalidFileData,
    FileEmpty,
    FileTooBig,
    OpenUploadFile,
    GetMetadata,
    ReadData,
    CreateFile,
    WriteFile,
    BlockedExtension,
    DescTooLong,
    AddFile,
}

#[derive(Debug)]
pub enum GetError {
    BlockedExtension,
    NotFound,
    FileNotFound,
}

#[derive(Debug)]
pub enum IdGetError {
    NotFound,
}

impl Flup {
    pub fn new(config: FlupConfig) -> Result<Flup, StartError> {
        let db = match FlupDb::new(config.redis_prefix.clone()) {
            Err(error) => return Err(StartError::Db(error)),
            Ok(db) => db,
        };

        Ok(Flup {
            config: config,
            db: db,
        })
    }

    fn new_file_id(&self) -> String {
        let mut i = self.db.new_id_seed().unwrap();
        let mut id = String::new();

        while i > 0 {
            id.push(ID_CHARS[i % ID_CHARS.len()]);
            i = i / ID_CHARS.len();
        }

        match self.db.get_file_by_id(id.clone()) {
            Ok(_) => self.new_file_id(),
            Err(_) => id,
        }
    }

    pub fn upload(&self, req: UploadRequest) -> Result<Vec<FileInfo>, UploadError> {
        guard!(let Some(params) = req.params else {
            return Err(UploadError::NoPostParams);
        });

        let ip = match req.xforwarded {
            Some(ref ips_string) if self.config.xforwarded == true => {
                handle_xforwarded(ips_string.clone(), self.config.xforwarded_index).expect("Invalid xforwarded index, change your config?")
            },
            _ => req.ip,
        };

        let uploader = hash_ip(self.config.salt.clone(), ip.clone());

        let desc = match params.desc {
            Some(ref desc) if desc.len() > 100 => {
                return Err(UploadError::DescTooLong);
            },
            Some(desc) => desc,
            _ => String::new(),
        };

        let mut files = vec![];

        for (file, filename) in params.files {
            guard!(let Some(mut file) = file else {
                return Err(UploadError::OpenUploadFile);
            });

            let file_size = match file.metadata() {
                Ok(file_metadata) => file_metadata.len(),
                _ => return Err(UploadError::GetMetadata),
            };

            if file_size == 0 {
                return Err(UploadError::FileEmpty);
            } else if file_size > 8388608 {
                return Err(UploadError::FileTooBig);
            }

            let mut file_data = vec![];
            if let Err(_) = file.read_to_end(&mut file_data) {
                return Err(UploadError::ReadData);
            }

            let hash = hash_file_data(&file_data);

            let file_info = match self.db.get_file_id_by_hash(hash.clone()) {
                Ok(file_id) => {
                    self.db.get_file_by_id(file_id.to_string()).expect("File with identical hash missing")
                },
                Err(_) => {
                    let file_id = match params.is_private {
                        true => hash.chars().take(10).collect(),
                        false => self.new_file_id(),
                    };

                    match File::create(format!("files/{}", file_id)) {
                        Ok(mut file) => {
                            match file.write_all(&file_data) { 
                                Err(_) => return Err(UploadError::WriteFile),
                                _ => {  },
                            }
                        },
                        Err(_) => return Err(UploadError::CreateFile),
                    }

                    let filename = match filename.as_ref().map(|filename| filename.trim())  {
                        Some(ref filename) if filename.len() != 0 => {
                            handle_filename(filename.to_string(), !params.no_filename)
                        },
                        _ => "file".to_string(),
                    };

                    if blocked_extension(&self.config.no_upload_extensions, &filename) {
                        return Err(UploadError::BlockedExtension);
                    }

                    let file_info = FileInfo {
                        name: filename,
                        desc: desc.clone(),
                        file_id: file_id.clone(),
                        hash: hash,
                        size: file_size,
                        uploader: uploader.clone(),
                    };

                    if let Err(_) = self.db.add_file(file_id, file_info.clone(), !params.is_private) {
                        return Err(UploadError::AddFile);
                    }

                    file_info
                },
            };

            files.push(file_info);
        }

        Ok(files)
    }

    pub fn file_by_id(&self, req: IdGetRequest) -> Result<(String, FileInfo), IdGetError> {
        guard!(let Ok(file_info) = self.db.get_file_by_id(req.file_id.clone()) else {
            return Err(IdGetError::NotFound);
        });

        Ok((req.file_id, file_info))
    }

    pub fn file(&self, req: GetRequest) -> Result<FileInfo, GetError> {
        if blocked_extension(&self.config.no_access_extensions, &req.filename) {
            return Err(GetError::BlockedExtension);
        }

        guard!(let Ok(file_info) = self.db.get_file_by_id(req.file_id.clone()) else {
            return Err(GetError::NotFound);
        });

        Ok(file_info)
    }

    pub fn uploads_count(&self) -> Result<(isize, isize), ()> {
        let uploads_count = match self.db.get_uploads_count() {
            Ok(count) => count,
            Err(_) => return Err(())
        };

        let public_uploads_count = match self.db.get_public_uploads_count() {
            Ok(count) => count,
            Err(_) => return Err(())
        };

        Ok((uploads_count, public_uploads_count))
    }

    pub fn public_uploads(&self) -> Result<Vec<FileInfo>, ()> {
        match self.db.get_public_uploads() {
            Ok(files) => Ok(files.into_iter().take(50).collect()),
            Err(_) => Err(()),
        }
    }

    pub fn deletion_log(&self) -> Result<Vec<DeletedFile>, ()> {
        match self.db.get_deleted_files() {
            Ok(deleted_files) => Ok(deleted_files),
            Err(_) => Err(()),
        }
    }
}

fn get_config() -> FlupConfig {
    let mut f = File::open("config.toml").expect("Config file missing");
    let mut data = String::new();
    f.read_to_string(&mut data).expect("Unable to read config file");

    let v = toml::Parser::new(&data).parse().expect("Error parsing config");
    let mut d = toml::Decoder::new(toml::Value::Table(v));
    FlupConfig::decode(&mut d).expect("Error creating FlupConfig from config")
}

fn main() {
    let config = get_config();

    let flup = match Flup::new(config.clone()) {
        Err(error) => panic!("Error starting: {:?}", error),
        Ok(flup) => flup,
    };

    FlupHandler::start(flup);
}
