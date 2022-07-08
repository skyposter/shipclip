//ffmpeg -f video4linux2 -s 640x480 -i /dev/video0 -ss 0:0:2 -frames 1 /tmp/out.jpg
#[macro_use]
extern crate rocket;

use chrono::{Datelike, Utc};
use regex::Regex;
use rocket::form::Form;
use rocket::fs::{relative, FileServer};
use rocket::http::{ContentType, Cookie, CookieJar};
use rocket::response::Redirect;
use rocket::serde::Serialize;
use rocket::tokio::task::spawn_blocking;
use rocket::State;
use rocket_dyn_templates::Template;
use std::env;
use std::io::prelude::*;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Mutex;
use std::thread;

static SAVEDIR: &str = relative!("saved");
static SAVELEN: usize = SAVEDIR.len();

struct Sendy(Mutex<Sender<bool>>);

#[derive(Serialize)]
struct Folder {
    name: String,
    path: String,
}

#[derive(Serialize)]
struct USBContext {
    file: Vec<Folder>,
    drive: Vec<Folder>,
}

#[derive(Serialize)]
struct SearchContext {
    message: String,
    dir: Vec<Folder>,
}

#[derive(FromForm)]
struct Signal<'r> {
    shipment: &'r str,
}

#[derive(FromForm)]
struct Transfer {
    source: Option<Vec<String>>,
    drive: Option<String>,
    delete: Option<String>,
}

#[derive(FromForm)]
struct USB {
    drive: String,
}

#[get("/")]
fn index() -> Redirect {
    Redirect::to("/public")
}

#[post("/save", data = "<form>")]
fn save_file(form: Form<Signal<'_>>, tx: &State<Sendy>) -> Redirect {
    let re = Regex::new(r"[^A-Za-z0-9]").unwrap();
    let name = re.replace_all(form.shipment, "x").as_ref().to_string();

    //send message to thread;
    if name.len() < 1 {
        return Redirect::to("/public");
    }

    let tx_mt = &tx.0;
    let tx_ch = tx_mt.lock().expect("Unable to lock mutex");
    tx_ch.send(true).unwrap();
    drop(tx_ch);

    let metadata = std::fs::metadata(relative!("static/latest.jpg"))
        .expect("unable to read metadata from file");

    let mut mtime = match metadata.modified() {
        Ok(t) => t,
        Err(_) => std::time::SystemTime::now(),
    };

    let mut wait_write = false;

    for _ in 0..40 {
        let newmeta = match std::fs::metadata(relative!("static/latest.jpg")) {
            Ok(m) => m,
            Err(_) => metadata.clone(),
        };

        let newtime = match newmeta.modified() {
            Ok(t) => t,
            Err(_) => mtime,
        };

        if mtime == newtime && wait_write {
            break;
        }

        if mtime != newtime {
            wait_write = true;
            mtime = newtime;
        }

        thread::sleep(std::time::Duration::from_millis(25));
    }

    spawn_blocking(move || save_file_to_folder(name));

    Redirect::to("/public")
}

#[get("/usb", rank = 2)]
fn search(jar: &CookieJar<'_>) -> Template {
    let messages = match jar.get("message").map(|cookie| cookie.value()) {
        Some(v) => v.to_string(),
        None => String::new(),
    };

    jar.remove(Cookie::named("message"));

    return Template::render(
        "search",
        &SearchContext {
            message: messages,
            dir: get_folder_content(SAVEDIR),
        },
    );
}

#[get("/usb?<search>")]
fn filetransfer(search: String) -> Template {
    let files = get_folder_content(&format!("{}/{}", SAVEDIR, search));
    let drives = get_folder_content("/media/");

    return Template::render(
        "browse",
        &USBContext {
            file: files,
            drive: drives,
        },
    );
}

#[get("/fulltransfer")]
fn full_select_usb() -> Template {
    let drives = get_folder_content("/media/");

    return Template::render(
        "select_usb",
        USBContext {
            drive: drives,
            file: Vec::new(),
        },
    );
}

#[get("/image?<file>")]
fn get_image(file: String, jar: &CookieJar<'_>) -> (ContentType, Vec<u8>) {
    use std::fs::File;

    let mut data: Vec<u8> = Vec::new();
    if file.replace("//", "/")[0..SAVELEN] != SAVEDIR[0..SAVELEN] {
        jar.add(Cookie::new("message", "Invalid path"));
        return (ContentType::JPEG, data);
    }

    let sourcepath = Path::new(&file);
    let mut f = File::open(sourcepath).expect("unable to open image file");
    f.read_to_end(&mut data).expect("Cant read from file");

    (ContentType::JPEG, data)
}

#[post("/fulltransfer", data = "<form>")]
fn full_submit(form: Form<USB>, jar: &CookieJar<'_>) -> Redirect {
    if form.drive[0..7] != "/media/"[0..7] {
        jar.add(Cookie::new("message", "Invalid path"));
        return Redirect::to("/usb");
    }
    let target = Path::new(&form.drive);
    let folder = get_folder_content(SAVEDIR);

    for f1 in folder {
        let files = get_folder_content(&f1.path);
        match std::fs::create_dir(target.join(&f1.name)) {
            Ok(_) => (),
            Err(_) => (),
        };

        for f2 in files {
            match std::fs::copy(f2.path, target.join(&f1.name).join(&f2.name)) {
                Ok(_) => (),
                Err(e) => jar.add(Cookie::new(
                    "message",
                    format!("unable to copy one or more files: {:?}", e),
                )),
            }
        }
    }
    return Redirect::to("/usb");
}

#[post("/usb/submit", data = "<form>")]
fn transfer_files(form: Form<Transfer>, jar: &CookieJar<'_>) -> Redirect {
    match &form.delete {
        Some(s) => {
            delete_file(s);
            return Redirect::to("/usb");
        }
        None => (),
    }

    let drive = match &form.drive {
        Some(s) => s,
        None => return Redirect::to("/usb"),
    };

    let source = match &form.source {
        Some(s) => s,
        None => return Redirect::to("/usb"),
    };

    if drive[0..7] != "/media/"[0..7] {
        jar.add(Cookie::new("message", "Invalid path"));
        return Redirect::to("/usb");
    }

    let target = Path::new(&drive);

    for f in source {
        let sourcepath = Path::new(&f);
        let filename = sourcepath.file_name().expect("Not a file");
        let parentfolder = sourcepath
            .parent()
            .expect("unable to find parent")
            .file_name()
            .expect("not a directory");
        let targetpath = target.join(format!(
            "{}-{}",
            parentfolder.to_str().unwrap(),
            filename.to_str().unwrap()
        ));

        if f.replace("//", "/")[0..SAVELEN] != SAVEDIR[0..SAVELEN] {
            jar.add(Cookie::new("message", "Invalid path"));
            continue;
        }

        match std::fs::copy(sourcepath, &targetpath) {
            Ok(o) => o,
            Err(e) => {
                jar.add(Cookie::new(
                    "message",
                    format!(
                        "Could not copy file {} to {}\n{:?}",
                        f,
                        targetpath.display(),
                        e
                    ),
                ));
                0
            }
        };
    }
    Redirect::to("/usb")
}

fn delete_file(file: &str) {
    if file.replace("//", "/")[0..SAVELEN] != SAVEDIR[0..SAVELEN] {
        return;
    }

    let sourcepath = Path::new(&file);
    std::fs::remove_file(&sourcepath).expect("Unable to delete file");

    let parentfolder = sourcepath.parent().expect("cant find parent folder!");
    let mut par_iter = parentfolder
        .read_dir()
        .expect("unable to read parent folder");
    if par_iter.next().is_none() {
        std::fs::remove_dir(parentfolder).expect("unable to delete parentfolder");
    }
}

fn get_folder_content(target: &str) -> Vec<Folder> {
    let re = Regex::new(r"[^A-Za-z0-9/]").unwrap();
    let safetarget = re.replace_all(target, "x");
    let path = Path::new(safetarget.as_ref());

    if path.exists() {
        let mut files = match std::fs::read_dir(path) {
            Ok(f) => f.peekable(),
            Err(_) => {
                println!("Unable to read dir");
                return Vec::new();
            }
        };

        let mut vec = Vec::new();
        let requestpath = safetarget.as_ref().replace("//", "/");

        if requestpath.len() > SAVELEN
            && requestpath[0..SAVELEN] == SAVEDIR[0..SAVELEN]
            && files.peek().is_none()
        {
            std::fs::remove_dir(path).expect("unable to delete parentfolder");
            return vec;
        }

        for file in files {
            let filename = file.unwrap().path();
            vec.push(Folder {
                name: filename
                    .file_name()
                    .expect("cant read")
                    .to_string_lossy()
                    .to_string(),
                path: filename.display().to_string(),
            });
        }
        vec.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        return vec;
    } else {
        return Vec::new();
    }
}

fn save_file_to_folder(name: String) {
    let re = Regex::new(r"[^A-Za-z0-9/]").unwrap();
    let safename = re.replace_all(&name, "x");
    let now = Utc::now();
    let (_, year) = now.year_ce();
    let date = format!(
        "{}-{:02}-{:02}_{}",
        year,
        now.month(),
        now.day(),
        now.time()
    )[0..19]
        .to_string()
        .replace(":", ".");
    let dir_path = format!("{}/{}", SAVEDIR, safename);
    let filename = PathBuf::from(format!("{}/{}.jpg", &dir_path, date));
    let source = PathBuf::from(relative!("/static/latest.jpg"));

    std::fs::create_dir_all(&dir_path).expect("unable to create dir");
    std::fs::copy(source, filename).expect("failed to save capture");
}

fn capture(rx: Receiver<bool>) {
    use rscam::{Camera, Config, ResolutionInfo};

    let mut camera = Camera::new("/dev/video0").expect("Unable to open video device");
    let res = camera
        .resolutions(b"RGB3")
        .expect("Could not find any resolutions for the current format");

    let best_res = match res {
        ResolutionInfo::Stepwise {
            min: _,
            max: (x, y),
            step: _,
        } => (x, y),
        _ => (1920, 1080),
    };

    camera
        .start(&Config {
            interval: (1, 15),
            resolution: best_res,
            format: b"RGB3",
            ..Default::default()
        })
        .expect("unable to start camera feed");

    loop {
        match rx.try_recv() {
            Ok(_) => {
                use image::ImageEncoder;
                let frame = camera.capture().unwrap();
                //let mut file = std::fs::File::create("/tmp/latest.jpg").unwrap();
                //file.write_all(&frame[..]).unwrap();

                let (x, y) = best_res;

                let mut i_buf = Vec::new();
                let encoder = image::codecs::jpeg::JpegEncoder::new(&mut i_buf);

                encoder
                    .write_image(&frame[..], x, y, image::ColorType::Rgb8)
                    .expect("unable to encode image");

                let mut cropme =
                    image::load_from_memory(&mut i_buf).expect("Could not determine image format");

                let (s_x, s_y, width, height) = get_crop_pixels(x, y);
                let cropped = cropme.crop(s_x, s_y, width, height);

                let dest = PathBuf::from(relative!("/static/latest.jpg"));
                //std::fs::copy(&source, dest).expect("failed to copy capture");
                cropped.save(dest).expect("Unable to write image to file!");

                //let source = PathBuf::from("/tmp/latest.jpg");
                //std::fs::remove_file(source).unwrap();
            }
            Err(_) => {
                camera.capture().unwrap();
            }
        }
    }
}

fn get_crop_pixels(x: u32, y: u32) -> (u32, u32, u32, u32) {
    let crop_top: u32;
    let crop_left: u32;
    let crop_x: u32;
    let crop_y: u32;

    if x > y {
        let diff = x - y;
        crop_left = diff / 2;
        crop_x = x - diff;
        crop_top = 0;
        crop_y = y;
    } else if x < y {
        let diff = y - x;
        crop_top = diff / 2;
        crop_y = y - diff;
        crop_left = 0;
        crop_x = x;
    } else {
        crop_left = 0;
        crop_top = 0;
        crop_x = x;
        crop_y = y;
    }

    (crop_left, crop_top, crop_x, crop_y)
}

fn fake_capture(rx: Receiver<bool>) {
    loop {
        match rx.try_recv() {
            Ok(_) => {
                println!("got a capture request!");
                let source = PathBuf::from(relative!("/static/latest.jpg"));
                let dest = PathBuf::from("/tmp/latest.jpg");
                std::fs::copy(&source, dest).expect("failed to copy capture");
            }
            Err(_) => (),
        }
    }
}

#[rocket::main]
async fn main() {
    let args: Vec<String> = env::args().collect();

    let (tx, rx): (Sender<bool>, Receiver<bool>) = mpsc::channel();
    let mutex_tx = Sendy(Mutex::new(tx));
    let camthread;

    if args.len() > 1 && args[1] == "test" {
        camthread = thread::spawn(move || fake_capture(rx));
    } else {
        camthread = thread::spawn(move || capture(rx));
    }

    let rock_rs = rocket::build()
        .manage(mutex_tx)
        .mount(
            "/",
            routes![
                index,
                save_file,
                filetransfer,
                transfer_files,
                search,
                full_submit,
                full_select_usb,
                get_image
            ],
        )
        .mount("/public", FileServer::from(relative!("static")))
        .attach(Template::fairing())
        .ignite()
        .await
        .expect("Unable to ignite Rocket!");
    let shutdown_handle = rock_rs.shutdown();

    rocket::tokio::spawn(rock_rs.launch());

    match camthread.join() {
        Ok(_) => println!("Thread returned from operation"),
        Err(e) => println!("Thread panic detected: {:?}", e),
    }
    shutdown_handle.notify();
}
