use clap::{App, Arg};
use dirs;
use failure::{err_msg, Error};
use glutin::Icon;
use glutin_window::GlutinWindow as Window;
use graphics::{DrawState, Graphics, Image, ImageSize, Transformed};
use image;
use image_grid::grid::{Color, Grid, TileAction, TileHandler};
use opengl_graphics::{GlGraphics, OpenGL, Texture, TextureSettings};
use piston::event_loop::*;
use piston::input::{
    keyboard::{Key, ModifierKey},
    mouse::MouseButton,
    Button, MouseCursorEvent, MouseScrollEvent, PressEvent, ReleaseEvent, RenderArgs, RenderEvent,
    UpdateEvent,
};
use piston::window::WindowSettings;
use reqwest;
use serde::{Deserialize, Serialize};
use serde_json;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command};
use steam::{app_info::AppInfo, steam_game::SteamGame};
use twitch::{TwitchDb, TwitchGame};
use url::Url;

#[derive(Deserialize, Serialize, Clone)]
enum ImageSource {
    Url(String),
    Path(String),
}

#[derive(Deserialize, Serialize)]
struct Game {
    id: String,
    title: String,
    image_path: Option<PathBuf>,
    image_src: ImageSource,
    installed: bool,
    kids: Option<bool>,
    players: Option<usize>,
    launch_url: Option<String>,
    install_directory: Option<String>,
    working_subdir_override: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
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

    fn read_img(&self, full_path: &PathBuf) -> Result<Vec<u8>, Error> {
        Ok(fs::read(&full_path)?)
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
                    .expect("Unable to launch game"),
            );
            let full_command =
                PathBuf::from(install_directory.join(self.command.as_ref().unwrap()));
            let mut launch = Command::new(&full_command);
            if self.working_subdir_override.is_some() {
                launch.current_dir(
                    install_directory.join(self.working_subdir_override.as_ref().unwrap()),
                );
            } else {
                launch.current_dir(install_directory);
            }
            launch.args(self.args.as_ref().unwrap());
            return Ok(launch.spawn()?);
        }
        if self.launch_url.is_some() {
            let mut launch = Command::new("cmd");
            launch.args(&["/C", "start", self.launch_url.as_ref().unwrap()]);
            return Ok(launch.spawn()?);
        }
        Err(err_msg("Unable to launch: Missing launch_url or command"))
    }
}

#[derive(Copy, Clone)]
enum DisplayFilter {
    All,
    Kids,
    Dad,
    NotInterested,
}

struct Doorways {
    games: Vec<Game>,
    display_filter: DisplayFilter,
    display_installed: Option<bool>,
    displayed_games: Vec<usize>,
    images: Vec<Texture>,
    image_folder: PathBuf,
    edit_mode: bool,
    allow_filter: bool,
    background_color: Option<Color>,
}

impl Doorways {
    fn new(cache_dir: PathBuf) -> Doorways {
        Doorways {
            games: Vec::new(),
            images: Vec::new(),
            image_folder: cache_dir.join("images"),
            display_filter: DisplayFilter::All,
            display_installed: None,
            displayed_games: Vec::new(),
            edit_mode: false,
            allow_filter: false,
            background_color: None,
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

    fn merge_with(mut self, other: Doorways) -> Doorways {
        let mut to_add: Vec<Game> = Vec::new();
        for orig in other.games.into_iter() {
            let mut found = false;
            for custom in &mut self.games {
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
                }
            }
            if !found {
                to_add.push(orig);
            }
        }
        self.games.extend(to_add);
        self
    }

    fn from_steam_games(image_folder: PathBuf, games: Vec<SteamGame>) -> Doorways {
        // Not able to do anything useful with uninstalled Steam game records yet.
        // Still need to figure out which ones are noise and which are not.
        let games = games
            .iter()
            .filter(|g| g.installed)
            .map(|g| Game {
                id: g.id.to_string(),
                title: g.title.clone(),
                image_src: ImageSource::Path(g.logo.as_ref().unwrap().clone()),
                installed: g.installed,
                launch_url: Some(format!("steam://rungameid/{}", g.id)),
                kids: None,
                players: None,
                command: None,
                args: None,
                image_path: None,
                install_directory: None,
                working_subdir_override: None,
            })
            .collect();
        let mut doorways = Doorways {
            games,
            display_filter: DisplayFilter::All,
            displayed_games: Vec::new(),
            display_installed: Some(true),
            images: Vec::new(),
            image_folder,
            edit_mode: false,
            allow_filter: false,
            background_color: None,
        };
        doorways.sort();
        doorways.update_filter(DisplayFilter::All);
        doorways
    }

    fn from_twitch_games(image_folder: PathBuf, games: &Vec<TwitchGame>) -> Doorways {
        let games = games
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
                players: None,
                image_path: None,
                launch_url: None,
            })
            .collect();
        let mut doorways = Doorways {
            games,
            display_filter: DisplayFilter::All,
            displayed_games: Vec::new(),
            display_installed: Some(true),
            images: Vec::new(),
            image_folder,
            edit_mode: false,
            allow_filter: false,
            background_color: None,
        };
        doorways.sort();
        doorways.update_filter(DisplayFilter::All);
        doorways
    }

    fn update_filter(&mut self, df: DisplayFilter) {
        self.display_filter = df;
        self.displayed_games = self
            .games
            .iter()
            .enumerate()
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
        let mut unloadable = Vec::new();
        for (index, game) in self.games.iter_mut().enumerate() {
            game.image_path = match &game.image_src {
                ImageSource::Url(_) => Some(game.download_img(&self.image_folder).unwrap()),
                ImageSource::Path(path) => Some(PathBuf::from(path)),
            };
            let texture = match Texture::from_path(
                &game.image_path.as_ref().unwrap(),
                &TextureSettings::new(),
            ) {
                Ok(t) => Ok(t),
                Err(msg) => Err(err_msg(msg)),
            };
            if texture.is_err() {
                println!("{}", game.title);
                unloadable.push(index);
                continue;
            }
            self.images.push(texture?);
        }
        for index in unloadable {
            self.games.remove(index);
        }
        Ok(self)
    }

    fn sort(&mut self) {
        // TODO: Track indexes rather than sorting in place?
        self.games
            .sort_unstable_by(|e1, e2| e1.title.cmp(&e2.title));
        self.images.clear();
    }
}

impl TileHandler for Doorways {
    fn tiles(&self) -> &Vec<usize> {
        &self.displayed_games
    }

    fn tile(&self, i: usize) -> &Texture {
        &self.images[i]
    }

    fn act(&self, i: usize) -> TileAction {
        TileAction::Launch(self.games[i].launch())
    }

    fn key_down(
        &mut self,
        i: usize,
        keycode: Key,
        keymod: ModifierKey,
    ) -> Option<(Key, ModifierKey)> {
        if self.edit_mode {
            match keycode {
                Key::K => {
                    self.games[i].kids = Some(true);
                    return None;
                }
                Key::D => {
                    self.games[i].kids = Some(false);
                    return None;
                }
                Key::U => {
                    self.games[i].kids = None;
                    return None;
                }
                _ => {}
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
                if keymod.contains(ModifierKey::SHIFT) {
                    self.display_installed = None;
                } else {
                    match self.display_installed {
                        None => self.display_installed = Some(true),
                        Some(value) => self.display_installed = Some(!value),
                    }
                }
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
}

fn main() -> Result<(), Error> {
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

    let home = dirs::home_dir().unwrap();
    let doorways_cache = home.join(".doorways");
    let image_folder = &doorways_cache.join("images");
    let config = dirs::config_dir().unwrap();
    let mut doorways = if !doorways_cache.join("games.json").exists() {
        Doorways::new(doorways_cache.clone())
    } else {
        Doorways::load(doorways_cache.clone())?
    };
    if matches.is_present("refresh") {
        eprintln!("Creating initial games list.");
        let app_infos = AppInfo::load()?;
        let games = SteamGame::from(&app_infos)?;
        doorways = doorways.merge_with(Doorways::from_steam_games(
            image_folder.to_path_buf(),
            games,
        ));
        let twitch_cache = home.join(".twitch");
        let twitch_db = TwitchDb::load(&twitch_cache)?;
        let games = TwitchGame::from_db(&twitch_db)?;
        doorways = doorways.merge_with(Doorways::from_twitch_games(
            image_folder.to_path_buf(),
            &games,
        ));
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

        // set_window_icon(Icon::from_path(
        //     PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("/doorways.bmp"),
        // ));
        let mut gl = GlGraphics::new(opengl);
        // TODO: Add support for downloading of images without loading into textures
        doorways.load_imgs()?;
        doorways.update_filter(DisplayFilter::Kids);
        let mut grid = Grid::new(Box::new(&mut doorways), 200, 200);
        grid.allow_draw_tile = false;
        grid.run(&mut window, &mut gl)?;
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
        println!("Unable to find game {}", game_to_launch);
        return Ok(());
    }

    Ok(())
}
