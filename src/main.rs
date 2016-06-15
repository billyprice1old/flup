#![feature(custom_derive, plugin)]
#![plugin(tojson_macros)]

#[macro_use] extern crate guard;
extern crate rustc_serialize;
extern crate iron;
extern crate router;
extern crate mount;
extern crate params;
extern crate handlebars_iron as hbs;
extern crate staticfile;
extern crate mime_guess;
extern crate toml;
extern crate crypto;

use rustc_serialize::Decodable;

use iron::prelude::*;
use iron::Url;
use iron::status::Status;
use iron::modifiers::Redirect;

use params::{Params, Value};
use mount::Mount;
use router::Router;

use hbs::{Template, HandlebarsEngine, DirectorySource};
use staticfile::Static;

use std::io::{self, Read};
use std::fs::File;
use std::path::Path;

use crypto::md5::Md5;
use crypto::digest::Digest;

mod file;
mod db;

use file::FlupFs;
use db::FlupDb;

fn hash_file(file: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.input(file);

    hasher.result_str()[..4].to_string()
}

fn hash_ip(salt: String, ip: String) -> String {
    let mut hasher = Md5::new();
    hasher.input(ip.as_bytes());
    hasher.input(salt.as_bytes());

    hasher.result_str()
}

#[derive(Debug, Clone, RustcDecodable)]
struct FlupConfig {
    host: String,
    url: String,

    salt: String,

    xforwarded: bool,
    xforwarded_index: usize,
}

#[derive(Debug, Clone, ToJson, RustcEncodable, RustcDecodable)]
pub struct FileInfo {
    name: String,
    desc: String,
    file_id: String,
    uploader: String,
}

#[derive(ToJson)]
struct HomePageData {
    uploads_count: isize,
    public_uploads_count: isize,
}

#[derive(ToJson)]
struct UploadsPageData {
    uploads: Vec<FileInfo>,
}

#[derive(ToJson)]
struct ErrorPageData {
    error: String,
}

#[derive(Clone)]
struct Flup {
    pub config: FlupConfig,
    pub db: FlupDb,
    pub fs: FlupFs,
}

#[derive(Clone)]
struct FlupHandler {
    flup: Flup,
}

#[derive(Debug)]
pub enum StartError {
    Redis(db::RedisError),
    Io(io::Error),
}

pub struct UploadRequestPost {
    file: Option<params::File>,

    public: bool,
    desc: Option<String>,
}

pub struct UploadRequest {
    xforwarded: Option<String>,

    post: Option<UploadRequestPost>,

    ip: String,
}

pub struct IdGetRequest {
    file_id: String,
}

pub struct GetRequest {
    file_id: String,
}

#[derive(Debug)]
pub enum UploadError {
    SetIp,
    NoPostParams,
    InvalidFileData,
    FileEmpty,
    FileTooBig,
    OpenUploadFile,
    ReadData,
    WriteFile,
    DescTooLong,
    AddFile,
}

#[derive(Debug)]
pub enum GetError {
    NotFound,
    FileNotFound
}

#[derive(Debug)]
pub enum IdGetError {
    NotFound,
}

impl Flup {
    pub fn new(config: FlupConfig) -> Result<Flup, StartError> {
        let db = try!(FlupDb::new());
        let fs = FlupFs::new();

        Ok(Flup {
            config: config,
            db: db,
            fs: fs,
        })
    }

    pub fn upload(&self, req: UploadRequest) -> Result<String, UploadError> {
        guard!(let Some(post) = req.post else {
            return Err(UploadError::NoPostParams);
        });

        let ip = match req.xforwarded {
            Some(ref ips_string) if self.config.xforwarded == true => {
                let mut ips: Vec<&str> = ips_string.split(", ").collect();
                ips.reverse();

                ips.get(self.config.xforwarded_index).unwrap().to_string()
            },
            _ => req.ip,
        };

        let uploader = hash_ip(self.config.salt.clone(), ip.clone());
        if let Err(_) = self.db.set_ip(uploader.clone(), ip) {
            return Err(UploadError::SetIp);
        }

        guard!(let Some(file) = post.file else {
            return Err(UploadError::InvalidFileData);
        });

        if file.size() == 0 {
            return Err(UploadError::FileEmpty);
        } else if file.size() > 8388608 {
            return Err(UploadError::FileTooBig);
        }

        let file_data = if let Ok(mut handle) = file.open() {
            let mut buf = vec![];

            if let Err(_) = handle.read_to_end(&mut buf) {
                return Err(UploadError::ReadData);
            }

            buf
        } else {
            return Err(UploadError::OpenUploadFile);
        };

        let file_id = hash_file(&file_data);

        if let Err(_) = self.db.get_file(file_id.to_string()) {
            if let Err(_) = self.fs.write_file(file_id.clone(), file_data) {
                return Err(UploadError::WriteFile);
            }

            let filename = match file.filename()  {
                Some(filename) if filename.len() != 0 => {
                    let path = Path::new(filename);

                    let base_str = path.file_stem().unwrap().to_str().unwrap();
                    let short_base: String = base_str.chars().take(45).collect();

                    match path.extension() {
                        Some(ext) => {
                            let ext_str = ext.to_str().unwrap();
                            let short_ext: String = ext_str.chars().take(10).collect();

                            format!("{}.{}", short_base, short_ext)
                        },
                        None => short_base,
                    }
                },
                _ => "file".to_string(),
            };

            let desc = match post.desc {
                Some(desc) => {
                    if desc.len() > 100 {
                        return Err(UploadError::DescTooLong);
                    }

                    desc
                },
                _ => "(none)".to_string(),
            };

            let file_info = FileInfo {
                name: filename,
                desc: desc.to_string(),
                file_id: file_id.clone(),
                uploader: uploader,
            };

            if let Err(_) = self.db.add_file(file_id.clone(), file_info.clone(), post.public) {
                return Err(UploadError::AddFile);
            }
        }

        Ok(file_id)
    }

    pub fn file_by_id(&self, req: IdGetRequest) -> Result<(String, FileInfo), IdGetError> {
        guard!(let Ok(file_info) = self.db.get_file(req.file_id.clone()) else {
            return Err(IdGetError::NotFound);
        });

        Ok((req.file_id, file_info))
    }

    pub fn file(&self, req: GetRequest) -> Result<(FileInfo, Vec<u8>), GetError> {
        guard!(let Ok(file_info) = self.db.get_file(req.file_id.clone()) else {
            return Err(GetError::NotFound);
        });

        guard!(let Ok(file_data) = self.fs.get_file(req.file_id.clone()) else {
            return Err(GetError::FileNotFound);
        });

        Ok((file_info, file_data))
    }
}

impl FlupHandler {
    pub fn new(flup: Flup) -> FlupHandler {
        FlupHandler {
            flup: flup,
        }
    }

    fn error_page(&self, status: Status, text: &str) -> IronResult<Response> {
        let data = ErrorPageData {
            error: text.to_string(),
        };

        let mut resp = Response::new();
        resp.set_mut(Template::new("error", data)).set_mut(status);
        Ok(resp)
    }

    pub fn handle_upload(&self, req: &mut Request) -> IronResult<Response> {
        let xforwarded = match req.headers.get_raw("X-Forwarded-For") {
            Some(data) if data.len() == 1 => {
                Some(String::from_utf8(data[0].clone()).unwrap())
            },
            _ => None,
        };

        let post = match req.get_ref::<Params>() {
            Ok(params) => {
                let file = match params.get("file") {
                    Some(&Value::File(ref file)) => Some(file.clone()),
                    _ => None,
                };

                let desc = match params.get("desc") {
                    Some(&Value::String(ref desc)) => Some(desc.clone()),
                    _ => None,
                };

                let is_public = match params.get("public") {
                    Some(&Value::String(ref toggle)) if toggle == "on" => true,
                    _ => false,
                };

                Some(UploadRequestPost {
                    file: file,

                    public: is_public,
                    desc: desc,
                })
            },
            _ => None,
        };

        let flup_req = UploadRequest {
            xforwarded: xforwarded,

            post: post,

            ip: req.remote_addr.to_string(),
        };

        match self.flup.upload(flup_req) {
            Ok(file_id) => {
                let url = format!("{}/{}", self.flup.config.url, file_id);
                Ok(Response::with((Status::Ok, format!("{}", url))))
            },
            Err(error) => {
                match error {
                    UploadError::SetIp => {
                        self.error_page(Status::InternalServerError, "Error adding IP to temp DB")
                    },
                    UploadError::NoPostParams => {
                        self.error_page(Status::BadRequest, "No POST params found")
                    },
                    UploadError::InvalidFileData => {
                        self.error_page(Status::BadRequest, "Invalid file data found")
                    },
                    UploadError::FileEmpty => {
                        self.error_page(Status::BadRequest, "Specified file is empty")
                    },
                    UploadError::FileTooBig => {
                        self.error_page(Status::BadRequest, "File exceeds our limit")
                    },
                    UploadError::OpenUploadFile => {
                        self.error_page(Status::InternalServerError, "Error opening uploaded file")
                    },
                    UploadError::ReadData => {
                        self.error_page(Status::InternalServerError, "Error reading file data")
                    },
                    UploadError::WriteFile => {
                        self.error_page(Status::InternalServerError, "Error writing to file")
                    },
                    UploadError::DescTooLong => {
                        self.error_page(Status::BadRequest, "Description too long")
                    },
                    UploadError::AddFile => {
                        self.error_page(Status::InternalServerError, "Error adding file to DB")
                    },
                }
            },
        }
    }

    pub fn handle_file_by_id(&self, req: &mut Request) -> IronResult<Response> {
        let router = req.extensions.get::<Router>().unwrap();
        let file_id = router.find("id").unwrap().to_string();

        let flup_req = IdGetRequest {
            file_id: file_id,
        };

        match self.flup.file_by_id(flup_req) {
            Ok((file_id, file_info)) => {
                let url = format!("{}/{}/{}", self.flup.config.url, file_id, file_info.name);
                Ok(Response::with((Status::SeeOther, Redirect(Url::parse(url.as_str()).unwrap()))))
            },
            Err(error) => {
                match error {
                    IdGetError::NotFound => {
                        self.error_page(Status::NotFound, "File not found")
                    }
                }
            },
        }
    }

    pub fn handle_file(&self, req: &mut Request) -> IronResult<Response> {
        let router = req.extensions.get::<Router>().unwrap();
        let file_id = router.find("id").unwrap().to_string();

        let flup_req = GetRequest {
            file_id: file_id,
        };

        match self.flup.file(flup_req) {
            Ok((file_info, file_data)) => {
                let mime = mime_guess::guess_mime_type(Path::new(file_info.name.as_str()));
                Ok(Response::with((Status::Ok, mime, file_data)))
            },
            Err(error) => {
                match error {
                    GetError::NotFound => {
                        self.error_page(Status::NotFound, "File not found")
                    },
                    GetError::FileNotFound => {
                        self.error_page(Status::NotFound, "File not found (on disk (oh fuck))")
                    },
                }
            },
        }
    }

    pub fn handle_home(&self, _: &mut Request) -> IronResult<Response> {
        let uploads_count = self.flup.db.get_uploads_count().unwrap_or(0);
        let public_uploads_count = self.flup.db.get_public_uploads_count().unwrap_or(0);

        let data = HomePageData {
            uploads_count: uploads_count,
            public_uploads_count: public_uploads_count,
        };

        let mut resp = Response::new();
        resp.set_mut(Template::new("index", data)).set_mut(Status::Ok);
        Ok(resp)
    }

    pub fn handle_uploads(&self, _: &mut Request) -> IronResult<Response> {
        let uploads = self.flup.db.get_uploads().unwrap_or(vec![]);

        let data = UploadsPageData {
            uploads: uploads,
        };

        let mut resp = Response::new();
        resp.set_mut(Template::new("uploads", data)).set_mut(Status::Ok);
        Ok(resp)
    }

    pub fn handle_about(&self, _: &mut Request) -> IronResult<Response> {
        let mut resp = Response::new();
        resp.set_mut(Template::new("about", ())).set_mut(Status::Ok);
        Ok(resp)
    }
}

fn get_config() -> FlupConfig {
    let mut f = File::open("config.toml").unwrap();
    let mut data = String::new();
    f.read_to_string(&mut data).unwrap();

    let v = toml::Parser::new(&data).parse().unwrap();
    let mut d = toml::Decoder::new(toml::Value::Table(v));
    FlupConfig::decode(&mut d).unwrap()
}

fn new_flup_router(flup_handler: FlupHandler) -> Router {
    let mut router = Router::new();

    {
        let flup_handler = flup_handler.clone();
        router.post("/", move |req: &mut Request| {
            flup_handler.handle_upload(req)
        });
    }

    {
        let flup_handler = flup_handler.clone();
        router.get("/:id", move |req: &mut Request| {
            flup_handler.handle_file_by_id(req)
        });
    }

    {
        let flup_handler = flup_handler.clone();
        router.get("/:id/*", move |req: &mut Request| {
            flup_handler.handle_file(req)
        });
    }

    {
        let flup_handler = flup_handler.clone();
        router.get("/uploads", move |req: &mut Request| {
            flup_handler.handle_uploads(req)
        });
    }

    {
        let flup_handler = flup_handler.clone();
        router.get("/about", move |req: &mut Request| {
            flup_handler.handle_about(req)
        });
    }

    {
        let flup_handler = flup_handler.clone();
        router.get("/", move |req: &mut Request| {
            flup_handler.handle_home(req)
        });
    }

    router
}

fn main() {
    let config = get_config();

    let mut hbse = HandlebarsEngine::new();
    hbse.add(Box::new(DirectorySource::new("./views/", ".hbs")));
    hbse.reload().unwrap();

    let flup = Flup::new(config.clone()).unwrap();
    let flup_handler = FlupHandler::new(flup);

    let mut mount = Mount::new();
    mount.mount("/", new_flup_router(flup_handler));
    mount.mount("/static/", Static::new(Path::new("static")));

    let mut chain = Chain::new(mount);
    chain.link_after(hbse);

    Iron::new(chain).http(config.host.as_str()).unwrap();
}
