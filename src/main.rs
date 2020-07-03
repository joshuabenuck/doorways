// #![windows_subsystem = "windows"]
// Uncomment to turn off console window completely.

use anyhow::{anyhow, Error, Result};
use clap::{App, Arg};
use dirs;
use epic::{EpicGame, EpicGames, EPIC_GAMES_JSON};
use glutin::Icon;
use glutin_window::GlutinWindow as Window;
use graphics::{math::Matrix2d, DrawState, Image, Transformed};
use image_grid::grid::{Color, Grid, TileHandler};
use kernel32;
use opengl_graphics::{GlGraphics, OpenGL, Texture, TextureSettings};
use piston::input::keyboard::{Key, ModifierKey};
use piston::window::{AdvancedWindow, WindowSettings};
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::ptr;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, sleep};
use std::time::Duration;
use steam::{app_info::AppInfo, package_info::PackageInfo, steam_game::SteamGame};
use twitch::{TwitchDb, TwitchGame};
use url::Url;
use user32;
use winapi;

const MAX_TILE_WIDTH: usize = 200;
const MAX_TILE_HEIGHT: usize = 200;

#[derive(Deserialize, Serialize, Clone)]
enum ImageSource {
    Url(String),
    Path(String),
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Hash, Clone, Copy)]
enum Launcher {
    Steam,
    Twitch,
    Epic,
    Unknown,
}

impl Default for Launcher {
    fn default() -> Self {
        Launcher::Unknown
    }
}

#[derive(Deserialize, Serialize)]
struct Game {
    id: String,
    title: String,
    image_path: Option<PathBuf>,
    image_src: ImageSource,
    installed: bool,
    kids: Option<bool>,
    hidden: Option<bool>,
    players: Option<usize>,
    launch_url: Option<String>,
    install_directory: Option<String>,
    working_subdir_override: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    #[serde(default)]
    launcher: Launcher,
}

impl Game {
    fn download_img(&self, path: &PathBuf) -> Result<PathBuf, Error> {
        assert!(path.exists(), "Path for image download does not exist!");
        let url = match &self.image_src {
            ImageSource::Url(raw_url) => raw_url,
            _ => panic!("download_img called without a url"),
        };
        let url = Url::parse(&url).expect("Unable to parse url for image");
        let filename = url
            .path_segments()
            .expect("Unable to segments from image url")
            .last()
            .expect("Unable to get filename from image url");
        let image = path.join(filename);
        if image.exists() {
            return Ok(image);
        }
        let mut resp = reqwest::get(url.as_str()).expect("Unable to retrieve image from url");
        assert!(resp.status().is_success());
        let mut buffer = Vec::new();
        resp.read_to_end(&mut buffer)?;
        fs::write(&image, buffer)?;
        Ok(image)
    }

    fn launch(&self) -> Result<Child, Error> {
        println!(
            "Launching {:?} {:?} {:?} {:?} {:?}",
            self.install_directory,
            self.working_subdir_override,
            self.command,
            self.args,
            self.launch_url
        );
        if self.install_directory.is_some() && self.command.is_some() {
            let install_directory = PathBuf::from(
                self.install_directory
                    .as_ref()
                    .expect("launch: Unable to get install directory"),
            );
            let full_command = PathBuf::from(
                install_directory.join(
                    self.command
                        .as_ref()
                        .expect("launch: Unable to get command"),
                ),
            );
            let mut launch = Command::new(&full_command);
            if self.working_subdir_override.is_some() {
                launch.current_dir(
                    install_directory.join(self.working_subdir_override.as_ref().unwrap()),
                );
            } else {
                launch.current_dir(install_directory);
            }
            if self.args.is_some() {
                launch.args(self.args.as_ref().unwrap());
            }
            return Ok(launch.spawn()?);
        }
        if self.launch_url.is_some() {
            let mut launch = Command::new("cmd");
            launch.args(&["/C", "start", self.launch_url.as_ref().unwrap()]);
            return Ok(launch.spawn()?);
        }
        Err(anyhow!("Unable to launch: Missing launch_url or command",))
    }
}

fn from_twitch(games: Vec<TwitchGame>) -> Vec<Game> {
    games
        .iter()
        .map(|g| Game {
            id: g.asin.to_string(),
            title: g.title.clone(),
            image_src: ImageSource::Url(g.image_url.clone()),
            installed: g.installed,
            install_directory: g.install_directory.clone(),
            working_subdir_override: g.working_subdir_override.clone(),
            command: g.command.clone(),
            args: g.args.clone(),
            kids: None,
            hidden: Some(false),
            players: None,
            image_path: None,
            launch_url: g.launch_url.clone(),
            launcher: Launcher::Twitch,
        })
        .collect()
}

fn from_steam(games: Vec<SteamGame>) -> Vec<Game> {
    // Not able to do anything useful with uninstalled Steam game records yet.
    // Still need to figure out which ones are noise and which are not.
    let games: Vec<Game> = games
        .iter()
        .filter(|g| !g.logo.is_none())
        .map(|g| Game {
            id: g.id.to_string(),
            title: g.title.clone(),
            image_src: ImageSource::Path(g.logo.as_ref().unwrap().clone()),
            installed: g.installed,
            launch_url: Some(format!("steam://rungameid/{}", g.id)),
            kids: None,
            hidden: Some(false),
            players: None,
            command: None,
            args: None,
            image_path: None,
            install_directory: None,
            working_subdir_override: None,
            launcher: Launcher::Steam,
        })
        .collect();
    println!("Steam -- {}", games.len());
    games
}

fn from_epic(games: Vec<EpicGame>) -> Vec<Game> {
    games
        .iter()
        .filter(|g| g.image_url.is_some())
        .map(|g| Game {
            id: g.display_name.clone(),
            title: g.display_name.clone(),
            image_src: ImageSource::Url(g.image_url.as_ref().unwrap().clone()),
            installed: true,
            install_directory: Some(g.install_location.clone()),
            working_subdir_override: None,
            command: Some(g.launch_executable.clone()),
            args: None,
            kids: None,
            hidden: Some(false),
            players: None,
            image_path: None,
            launch_url: None,
            launcher: Launcher::Epic,
        })
        .collect()
}

trait VecGame {
    fn merge_with(self, other: Vec<Game>) -> Self;
}

impl VecGame for Vec<Game> {
    fn merge_with(mut self, other: Vec<Game>) -> Self {
        let mut to_add: Vec<Game> = Vec::new();
        for orig in other.into_iter() {
            let mut found = false;
            for custom in self.iter_mut() {
                if orig.id == custom.id {
                    found = true;
                    custom.title = orig.title.clone();
                    custom.image_src = orig.image_src.clone();
                    custom.install_directory = orig.install_directory.clone();
                    custom.working_subdir_override = orig.working_subdir_override.clone();
                    custom.installed = orig.installed.clone();
                    custom.command = orig.command.clone();
                    custom.args = orig.args.clone();
                    custom.launch_url = orig.launch_url.clone();
                    custom.launcher = orig.launcher.clone();
                }
            }
            if !found {
                eprintln!("Added: {}", orig.title);
                to_add.push(orig);
            }
        }
        self.extend(to_add);
        self
    }
}

#[derive(Copy, Clone)]
enum DisplayFilter {
    All,
    Kids,
    Dad,
    NotInterested,
}

enum LaunchStatus {
    Starting,
    Running,
    Success,
    FailedToLaunch(Error),
    Error(i32),
}

struct Doorways {
    games: Vec<Game>,
    status: Arc<Mutex<HashMap<usize, LaunchStatus>>>,
    display_filter: DisplayFilter,
    display_installed: Option<bool>,
    displayed_games: Vec<usize>,
    images: Vec<Option<Texture>>,
    image_folder: PathBuf,
    edit_mode: bool,
    allow_filter: bool,
    background_color: Option<Color>,
    icons: HashMap<Launcher, Texture>,
    status_channel: Option<mpsc::Sender<(usize, Launched)>>,
    show_overlay: bool,
}

impl Doorways {
    fn new(cache_dir: PathBuf) -> Doorways {
        let icons = HashMap::new();
        Doorways {
            games: Vec::new(),
            status: Arc::new(Mutex::new(HashMap::new())),
            images: Vec::new(),
            image_folder: cache_dir.join("images"),
            display_filter: DisplayFilter::All,
            display_installed: Some(true),
            displayed_games: Vec::new(),
            edit_mode: false,
            allow_filter: false,
            background_color: None,
            icons,
            status_channel: None,
            show_overlay: true,
        }
    }

    fn load(cache_dir: PathBuf) -> Result<Doorways, Error> {
        let games: Vec<Game> =
            serde_json::from_str(fs::read_to_string(cache_dir.join("games.json"))?.as_str())?;
        let mut doorways = Doorways::new(cache_dir);
        doorways.games = games;
        doorways.sort();
        doorways.update_filter(DisplayFilter::All);
        Ok(doorways)
    }

    fn save(&self, cache_dir: &PathBuf) -> Result<(), Error> {
        fs::File::create(cache_dir.join("games.json"))?
            .write(serde_json::to_string_pretty(&self.games)?.as_bytes())?;
        Ok(())
    }

    fn update_filter(&mut self, df: DisplayFilter) {
        self.display_filter = df;
        self.displayed_games = self
            .games
            .iter()
            .enumerate()
            .filter(|(_i, g)| !g.hidden.unwrap_or(false))
            .filter(|(_i, g)| match self.display_installed {
                Some(value) => g.installed == value,
                None => true,
            })
            .filter(|(_i, g)| match &self.display_filter {
                DisplayFilter::All => true,
                DisplayFilter::Dad => !g.kids.unwrap_or(true),
                DisplayFilter::Kids => g.kids.unwrap_or(false),
                DisplayFilter::NotInterested => g.kids == None,
            })
            .map(|(i, _g)| i)
            .collect();
    }

    fn load_imgs(&mut self) -> Result<&Doorways, Error> {
        for (_index, game) in self.games.iter_mut().enumerate() {
            game.image_path = match &game.image_src {
                ImageSource::Url(_) => Some(game.download_img(&self.image_folder).unwrap()),
                ImageSource::Path(path) => Some(PathBuf::from(path)),
            };
            let contents =
                std::fs::read(game.image_path.as_ref().unwrap()).expect("Unable to read file");
            let img = match image::load_from_memory(&contents) {
                Ok(t) => Ok(t),
                Err(msg) => {
                    eprintln!("Unable to load: {}; {}", game.title, msg);
                    Err(anyhow!(msg))
                }
            };
            if img.is_err() {
                game.hidden = Some(true);
                self.images.push(None);
                continue;
            }
            let img = match img.unwrap() {
                image::DynamicImage::ImageRgba8(img) => img,
                x => x.to_rgba(),
            };
            // Resize to reduce GPU memory consumption
            let scale = f32::min(
                MAX_TILE_WIDTH as f32 / img.width() as f32,
                MAX_TILE_HEIGHT as f32 / img.height() as f32,
            );
            let img = image::imageops::resize(
                &img,
                (img.width() as f32 * scale) as u32,
                (img.height() as f32 * scale) as u32,
                image::imageops::FilterType::Gaussian,
            );

            let texture = Texture::from_image(&img, &TextureSettings::new());
            self.images.push(Some(texture));
        }
        Ok(self)
    }

    fn sort(&mut self) {
        // TODO: Track indexes rather than sorting in place?
        self.games
            .sort_unstable_by(|e1, e2| e1.title.cmp(&e2.title));
        self.images.clear();
    }

    fn icon(&self, i: usize) -> Option<&Texture> {
        self.icons.get(&self.games[i].launcher)
    }

    fn start_status_thread(&mut self) {
        if self.status_channel.is_some() {
            return ();
        }
        let (tx, rx) = mpsc::channel::<(usize, Launched)>();
        self.status_channel = Some(tx);
        let status = self.status.clone();
        thread::spawn(move || {
            ChildMonitor::new(rx, status).process();
        });
    }
}

struct Launched {
    child: Child,
    launcher: Launcher,
    id: String,
}

struct ChildMonitor {
    active: HashMap<usize, Launched>,
    rx: mpsc::Receiver<(usize, Launched)>,
    status: Arc<Mutex<HashMap<usize, LaunchStatus>>>,
}

fn steam_status(id: &str) -> Result<LaunchStatus, Error> {
    use winreg::enums::*;
    use winreg::RegKey;
    let key = format!(r"Software\Valve\Steam\Apps\{}", id);
    // look up regkey
    let hklm = RegKey::predef(HKEY_CURRENT_USER);
    let app = hklm.open_subkey(key)?;
    let running: u32 = app.get_value("Running")?;
    if running == 0x01 {
        Ok(LaunchStatus::Running)
    } else {
        Ok(LaunchStatus::Success)
    }
}

impl ChildMonitor {
    fn new(
        rx: mpsc::Receiver<(usize, Launched)>,
        status: Arc<Mutex<HashMap<usize, LaunchStatus>>>,
    ) -> ChildMonitor {
        ChildMonitor {
            active: HashMap::new(),
            rx,
            status,
        }
    }

    fn poll_active(&mut self) {
        let mut to_remove = Vec::<usize>::new();
        for (i, launched) in self.active.iter_mut() {
            match launched.child.try_wait() {
                Ok(Some(exit_status)) => {
                    let status = if exit_status.success() {
                        if launched.launcher == Launcher::Steam {
                            match steam_status(&launched.id) {
                                Err(msg) => {
                                    eprintln!("Error getting steam status: {}", msg);
                                    LaunchStatus::Error(1)
                                }
                                Ok(status) => status,
                            }
                        } else {
                            LaunchStatus::Success
                        }
                    } else {
                        LaunchStatus::Error(exit_status.code().expect("Unable to get exit code"))
                    };
                    match status {
                        LaunchStatus::Running => {}
                        _ => {
                            to_remove.push(*i);
                        }
                    }
                    self.status.lock().unwrap().insert(*i, status);
                }
                Ok(None) => {
                    self.status
                        .lock()
                        .unwrap()
                        .insert(*i, LaunchStatus::Running);
                }
                Err(err) => panic!("Error waiting on child: {}", err),
            }
        }
        for i in to_remove {
            self.active.remove(&i).expect("Unable to remove.");
        }
    }

    fn process(&mut self) {
        loop {
            match self.rx.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    self.poll_active();
                    sleep(Duration::from_secs(1));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Should never happen.
                    panic!("Unexpected disconnection");
                }
                Ok((i, child)) => {
                    self.active.insert(i, child);
                }
            }
        }
    }
}

impl TileHandler for Doorways {
    fn window_title(&self) -> String {
        let lock = match self.allow_filter {
            true => "🔑",
            false => "🔒",
        };
        let install_filter = match self.display_installed {
            None => "",
            Some(true) => "[Installed]",
            Some(false) => "[Not Installed]",
        };
        let filter = match &self.display_filter {
            DisplayFilter::All => "All",
            DisplayFilter::Dad => "Dad",
            DisplayFilter::Kids => "Kids",
            DisplayFilter::NotInterested => "Unknown",
        };
        let count = self.displayed_games.len();
        format!(
            "Doorways {} (Filter: {}{}{})",
            count, filter, install_filter, lock
        )
    }

    fn tiles(&self) -> &Vec<usize> {
        &self.displayed_games
    }

    fn tile(&self, i: usize) -> &Texture {
        self.images[i].as_ref().unwrap()
    }

    fn act(&mut self, i: usize) {
        {
            let mut status = self.status.lock().unwrap();
            // Explicitly enumerating to ensure how each case is handled makes sense.
            // If we are starting or the game is running, don't attempt to launch again.
            match status.get(&i) {
                Some(LaunchStatus::Starting) | Some(LaunchStatus::Running) => return,
                None
                | Some(LaunchStatus::Error(_))
                | Some(LaunchStatus::FailedToLaunch(_))
                | Some(LaunchStatus::Success) => {}
            };
            status.insert(i, LaunchStatus::Starting);
        }

        self.start_status_thread();
        match &self.status_channel {
            None => panic!("Unable to start status thread!"),
            Some(tx) => {
                let result = self.games[i].launch();
                match result {
                    Ok(child) => {
                        tx.send((
                            i,
                            Launched {
                                child,
                                launcher: self.games[i].launcher,
                                id: self.games[i].id.clone(),
                            },
                        ))
                        .unwrap_or_else(|err| panic!("Unable to send to thread: {}", err));
                        ()
                    }
                    Err(err) => {
                        self.status
                            .lock()
                            .unwrap()
                            .insert(i, LaunchStatus::FailedToLaunch(err));
                        ()
                    }
                };
            }
        };
    }

    fn key_down(
        &mut self,
        i: usize,
        keycode: Key,
        keymod: ModifierKey,
    ) -> Option<(Key, ModifierKey)> {
        let game_index = self.tiles()[i];
        if self.edit_mode {
            match keycode {
                Key::K => {
                    self.games[game_index].kids = Some(true);
                    return None;
                }
                Key::D => {
                    self.games[game_index].kids = Some(false);
                    return None;
                }
                Key::U => {
                    self.games[game_index].kids = None;
                    return None;
                }
                _ => {}
            }
        }

        if keymod.contains(ModifierKey::CTRL) {
            if keycode == Key::O {
                self.show_overlay = !self.show_overlay;
            }
        }
        if keymod.contains(ModifierKey::CTRL) {
            if keycode == Key::E {
                self.edit_mode = !self.edit_mode;
                self.background_color = None;
                if self.edit_mode {
                    self.background_color = Some([0.2, 0.0, 0.2, 1.0]);
                }
                return None;
            }
            if keycode == Key::F {
                self.allow_filter = !self.allow_filter;
                return None;
            }
        };

        if !self.allow_filter {
            return Some((keycode, keymod));
        }

        match keycode {
            Key::K => {
                self.update_filter(DisplayFilter::Kids);
            }
            Key::D => {
                self.update_filter(DisplayFilter::Dad);
            }
            Key::U => {
                self.update_filter(DisplayFilter::NotInterested);
            }
            Key::A => {
                self.update_filter(DisplayFilter::All);
            }
            Key::I => {
                self.display_installed = if keymod.contains(ModifierKey::SHIFT) {
                    None
                } else {
                    match self.display_installed {
                        None => Some(true),
                        Some(value) => Some(!value),
                    }
                };
                self.update_filter(self.display_filter);
            }
            _ => return Some((keycode, keymod)),
        }
        None
    }

    fn highlight_color(&self, i: usize) -> Color {
        if let Some(kids) = self.games[i].kids {
            if kids {
                return [0.0, 1.0, 0.0, 1.0];
            }
            // not kids
            return [1.0, 0.0, 0.0, 1.0];
        }
        // unknown
        return [0.5, 0.5, 0.5, 1.0];
    }

    fn background_color(&self) -> Color {
        self.background_color.unwrap_or([0.1, 0.2, 0.3, 1.0])
    }

    fn draw_tile(
        &self,
        i: usize,
        transform: Matrix2d,
        gl: &mut GlGraphics,
        target_width: usize,
        target_height: usize,
    ) {
        let image = self.tile(i);
        let (scale, width, height) = self.compute_size(&image, target_width, target_height);
        let x_image_margin = (target_width - width) / 2;
        let y_image_margin = (target_height - height) / 2;

        let state = DrawState::default();
        Image::new().draw(
            image,
            &state,
            transform
                .trans(x_image_margin as f64, y_image_margin as f64)
                .zoom(scale.into()),
            gl,
        );
        let (color, gray_out) = {
            let mut statuses = self.status.lock().unwrap();
            let status = statuses.get_mut(&i);
            if status.is_none() {
                return ();
            }
            let mut gray_out = false;
            let color = match status.unwrap() {
                LaunchStatus::Starting => {
                    gray_out = true;
                    [0.0, 0.0, 0.0, 0.4]
                }
                LaunchStatus::Running => [0.0, 1.0, 0.0, 1.0],
                LaunchStatus::Success => [1.0, 0.0, 1.0, 1.0],
                LaunchStatus::Error(_) => [1.0, 0.0, 0.0, 1.0],
                LaunchStatus::FailedToLaunch(_) => [0.8, 0.8, 0.8, 1.0],
            };
            (color, gray_out)
        };
        if gray_out {
            let transform = transform.trans(x_image_margin as f64, y_image_margin as f64);
            let rect = graphics::rectangle::Rectangle::new(color);
            rect.draw(
                [0.0, 0.0, width as f64, height as f64],
                &state,
                transform,
                gl,
            );
        }
        if self.show_overlay == false {
            return ();
        }
        match self.icon(i) {
            Some(icon) => {
                let (iscale, iwidth, iheight) = self.compute_size(icon, 20, 20);
                Image::new().draw(
                    icon,
                    &state,
                    transform
                        .trans(
                            (x_image_margin + width - iwidth as usize - 2) as f64,
                            (y_image_margin + height - iheight as usize - 2) as f64,
                        )
                        .zoom(iscale),
                    gl,
                );
            }
            None => {}
        }
        if gray_out {
            return ();
        }
        let transform = transform.trans(
            (x_image_margin + 3) as f64,
            (y_image_margin + height - 20 - 3) as f64,
        );
        //let rect = graphics::rectangle::Rectangle::new(color);
        //rect.draw([0.0, 0.0, 20.0, 20.0], &state, transform, gl);
        graphics::ellipse(color, [0.0, 0.0, 20.0, 20.0], transform, gl);
    }
}

fn hide_console_window() {
    let window = unsafe { kernel32::GetConsoleWindow() };
    // https://msdn.microsoft.com/en-us/library/windows/desktop/ms633548%28v=vs.85%29.aspx
    if window != ptr::null_mut() {
        unsafe { user32::ShowWindow(window, winapi::um::winuser::SW_HIDE) };
    }
}

fn main() -> Result<()> {
    let matches = App::new("doorways")
        .about("A unified launcher for common game libraries.")
        .arg(
            Arg::with_name("launcher")
                .long("launcher")
                .help("Display graphical launcher."),
        )
        .arg(
            Arg::with_name("installed")
                .long("installed")
                .short("i")
                .default_value("true")
                .help("Limit operations to just the installed games."),
        )
        .arg(
            Arg::with_name("list")
                .long("list")
                .help("List the known games."),
        )
        .arg(
            Arg::with_name("refresh")
                .long("refresh")
                .help("Refresh the list of games from source."),
        )
        .arg(
            Arg::with_name("launch")
                .long("launch")
                .short("l")
                .takes_value(true)
                .help("Launch the specified game."),
        )
        .get_matches();

    if matches.is_present("launcher") {
        hide_console_window();
    }
    let home = dirs::home_dir().unwrap();
    let doorways_cache = home.join(".doorways");
    let mut doorways = if !doorways_cache.join("games.json").exists() {
        Doorways::new(doorways_cache.clone())
    } else {
        Doorways::load(doorways_cache.clone())?
    };
    if matches.is_present("refresh") {
        eprintln!("Creating initial games list.");
        // Reset hidden status during refresh
        for game in doorways.games.iter_mut() {
            game.hidden = None;
        }
        let app_infos = AppInfo::load()?;
        let pkg_infos = PackageInfo::load()?;
        let steam = from_steam(SteamGame::from(&app_infos, &pkg_infos)?);
        eprintln!("Steam games: {}", steam.len());
        doorways.games = doorways.games.merge_with(steam);
        let twitch_cache = home.join(".twitch");
        let twitch_db = TwitchDb::load(&twitch_cache)?;
        let twitch = from_twitch(TwitchGame::from_db(&twitch_db)?);
        eprintln!("Twitch games: {}", twitch.len());
        doorways.games = doorways.games.merge_with(twitch);
        let epic_games = EpicGame::load(&home.join(".epic"))?;
        let epic = from_epic(epic_games);
        doorways.games = doorways.games.merge_with(epic);
    };

    if matches.is_present("launcher") {
        // Change this to OpenGL::V2_1 if not working.
        let opengl = OpenGL::V3_2;

        // Create an Glutin window.
        let mut window: Window = WindowSettings::new("Doorways", [800, 600])
            .resizable(true)
            .vsync(true)
            .graphics_api(opengl)
            .exit_on_esc(true)
            .build()
            .unwrap();
        let doorways_bytes = include_bytes!("../doorways.bmp");
        window
            .ctx
            .window()
            .set_window_icon(Some(Icon::from_bytes(doorways_bytes)?));
        window.ctx.window().set_maximized(true);
        let mut gl = GlGraphics::new(opengl);
        // TODO: Add support for downloading of images without loading into textures
        doorways.load_imgs()?;
        doorways.update_filter(DisplayFilter::Kids);
        let settings = TextureSettings::new().filter(texture::Filter::Linear);
        doorways.icons.insert(
            Launcher::Steam,
            Texture::from_image(
                &image::load_from_memory(include_bytes!("../steam.ico"))
                    .expect("Unable to load steam icon.")
                    .to_rgba(),
                &settings,
            ),
        );
        doorways.icons.insert(
            Launcher::Twitch,
            Texture::from_image(
                &image::load_from_memory(include_bytes!("../twitch.ico"))
                    .expect("Unable to load twitch icon.")
                    .to_rgba(),
                &settings,
            ),
        );
        doorways.icons.insert(
            Launcher::Epic,
            Texture::from_image(
                &image::load_from_memory(include_bytes!("../epic.ico"))
                    .expect("Unable to load epic icon.")
                    .to_rgba(),
                &settings,
            ),
        );
        doorways.icons.insert(
            Launcher::Unknown,
            Texture::from_image(
                &image::load_from_memory(include_bytes!("../win10.png"))
                    .expect("Unable to load win10 icon.")
                    .to_rgba(),
                &settings,
            ),
        );
        window.set_title(doorways.window_title());
        eprintln!("Current game count: {}", doorways.games.len());
        let mut grid = Grid::new(Box::new(&mut doorways), MAX_TILE_WIDTH, MAX_TILE_HEIGHT);
        grid.allow_draw_tile = false;
        grid.run(&mut window, &mut gl)?;
        eprintln!("Game count before save: {}", doorways.games.len());
        doorways.save(&doorways_cache)?;
        return Ok(());
    }

    if matches.is_present("list") {
        let installed_only = matches.value_of("installed").unwrap().parse::<bool>()?;
        for game in doorways.games {
            if installed_only && !game.installed {
                continue;
            }
            println!("{}", game.title);
        }
        return Ok(());
    }

    if let Some(game_to_launch) = matches.value_of("launch") {
        for game in doorways.games {
            // TODO: Support partial and case insensitive matching
            if game.title == game_to_launch {
                game.launch()?;
                return Ok(());
            }
        }
        eprintln!("Unable to find game {}", game_to_launch);
        return Ok(());
    }

    Ok(())
}
