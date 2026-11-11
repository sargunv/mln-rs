// Ports platform/glfw/main.cpp: the `mln-glfw` app. Without `--style` (and
// without MLN_STYLE_URL) it opens OpenFreeMap Liberty, as C++ does.
//
// Command line (C++ uses args.hxx; the port parses the same long and short
// options by hand): -h/--help, -f/--fullscreen, -b/--benchmark, -o/--offline,
// -s/--style URL, -t/--apikey KEY, -c/--cache FILE, -x/--lon, -y/--lat,
// -a/--alt, --fov, -z/--zoom, --bearing, -p/--pitch, -r/--roll,
// -P/--maxPitch. TODO(port): --testDir, --backend, --actionJournalDir,
// -F/--showFeatures (rendered feature info). C++ declares `-b` twice
// (benchmark and bearing) and `-F` twice (showFeatures and fov); the port
// gives the short forms to the flags and keeps --bearing/--fov long-only.
//
// Port-only options: --readback FILE.png (write the presented frame once
// the map is idle, see glfw_view.rs), --quit-after SECONDS,
// --gesture-test (scripted input through the view's event handlers) and
// --no-system-fonts (or MLN_GLFW_NO_SYSTEM_FONTS set): without it, the map
// falls back to system fonts for the scripts that glyph PBFs can't shape
// (system_fonts.rs).
//
// TODO(port): SIGINT handling (C++ closes the window; here Ctrl-C ends the
// process), the plugin registration, settings load/save, the action
// journal, resetting the database (`I`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mln_glfw::glfw_renderer_frontend::GLFWRendererFrontend;
use mln_glfw::glfw_view::GLFWView;
use mln_glfw::settings_json::SettingsJSON;
use mln_glfw::system_fonts;
use mln_map::{BoundOptions, CameraOptions, Map, MapDebugOptions, MapOptions};
use mln_renderer::Renderer;
use mln_storage::{ClientOptions, FileSourceManager, FileSourceType, ResourceOptions};
use mln_util::geo::LatLng;
use mln_util::logging::{Event, Log};
use mln_util::tile_server_options::TileServerOptions;

const USAGE: &str = "  mln-glfw {OPTIONS}

    MapLibre Native GLFW example

  OPTIONS:

      -h, --help                        Display this help menu
      -f, --fullscreen                  Toggle fullscreen
      -b, --benchmark                   Toggle benchmark
      -o, --offline                     Toggle offline
      -t[key], --apikey=[key]           API key
      -s[URL], --style=[URL]            Map stylesheet
      -c[file], --cache=[file]          Cache database file name
      -x[degrees], --lon=[degrees]      Longitude
      -y[degrees], --lat=[degrees]      Latitude
      -a[degrees], --alt=[degrees]      Altitude
      --fov=[degrees]                   FOV
      -z[number], --zoom=[number]       Zoom level
      --bearing=[degrees]               Bearing
      -p[degrees], --pitch=[degrees]    Pitch
      -r[degrees], --roll=[degrees]     Roll
      -P[degrees], --maxPitch=[degrees] Max Pitch
      --readback=[file.png]             (port) Write the frame once the map is idle
      --quit-after=[seconds]            (port) Close the window after this time
      --gesture-test                    (port) Replay scripted input, log the camera, quit
      --no-system-fonts                 (port) Don't fall back to system fonts for complex scripts
";

#[derive(Default)]
struct Args {
    fullscreen: bool,
    benchmark: bool,
    offline: bool,
    apikey: Option<String>,
    style: Option<String>,
    cache: Option<String>,
    lon: Option<f64>,
    lat: Option<f64>,
    alt: Option<f64>,
    fov: Option<f64>,
    zoom: Option<f64>,
    bearing: Option<f64>,
    pitch: Option<f64>,
    roll: Option<f64>,
    max_pitch: Option<f64>,
    readback: Option<String>,
    quit_after: Option<f64>,
    gesture_test: bool,
    no_system_fonts: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        // --name=value, --name value, -xvalue, -x value
        let (name, inline): (String, Option<String>) = if let Some(long) = arg.strip_prefix("--") {
            match long.split_once('=') {
                Some((n, v)) => (n.to_owned(), Some(v.to_owned())),
                None => (long.to_owned(), None),
            }
        } else if let Some(short) = arg.strip_prefix('-')
            && !short.is_empty()
        {
            let (n, v) = short.split_at(1);
            (n.to_owned(), (!v.is_empty()).then(|| v.to_owned()))
        } else {
            return Err(format!("Passed in argument, but no positional arguments were ready to receive it: {arg}"));
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline.clone().or_else(|| it.next()).ok_or_else(|| format!("Flag '{name}' requires an argument"))
        };
        let number = |s: String, name: &str| {
            s.parse::<f64>().map_err(|_| format!("Argument '{name}' received invalid value type '{s}'"))
        };
        match name.as_str() {
            "h" | "help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            "f" | "fullscreen" => args.fullscreen = true,
            "b" | "benchmark" => args.benchmark = true,
            "o" | "offline" => args.offline = true,
            "t" | "apikey" => args.apikey = Some(value(&name)?),
            "s" | "style" => args.style = Some(value(&name)?),
            "c" | "cache" => args.cache = Some(value(&name)?),
            "x" | "lon" => args.lon = Some(number(value(&name)?, &name)?),
            "y" | "lat" => args.lat = Some(number(value(&name)?, &name)?),
            "a" | "alt" => args.alt = Some(number(value(&name)?, &name)?),
            "fov" => args.fov = Some(number(value(&name)?, &name)?),
            "z" | "zoom" => args.zoom = Some(number(value(&name)?, &name)?),
            "bearing" => args.bearing = Some(number(value(&name)?, &name)?),
            "p" | "pitch" => args.pitch = Some(number(value(&name)?, &name)?),
            "r" | "roll" => args.roll = Some(number(value(&name)?, &name)?),
            "P" | "maxPitch" => args.max_pitch = Some(number(value(&name)?, &name)?),
            "readback" => args.readback = Some(value(&name)?),
            "quit-after" => args.quit_after = Some(number(value(&name)?, &name)?),
            "gesture-test" => args.gesture_test = true,
            "no-system-fonts" => args.no_system_fonts = true,
            _ => return Err(format!("Flag could not be matched: {name}")),
        }
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            eprintln!("{e}");
            eprint!("{USAGE}");
            std::process::exit(1);
        }
    };

    // Load settings
    let mut settings = SettingsJSON { online: !args.offline, ..Default::default() };
    if let Some(v) = args.lon {
        settings.longitude = v;
    }
    if let Some(v) = args.lat {
        settings.latitude = v;
    }
    if let Some(v) = args.alt {
        settings.altitude = v;
    }
    if let Some(v) = args.fov {
        settings.fov = v;
    }
    if let Some(v) = args.max_pitch {
        settings.max_pitch = v;
    }
    if let Some(v) = args.zoom {
        settings.zoom = v;
    }
    if let Some(v) = args.bearing {
        settings.bearing = v;
    }
    if let Some(v) = args.pitch {
        settings.pitch = v;
    }
    if let Some(v) = args.roll {
        settings.roll = v;
    }

    // Finds the system fonts while the window opens.
    let font_discovery = (!args.no_system_fonts && std::env::var_os("MLN_GLFW_NO_SYSTEM_FONTS").is_none())
        .then(|| std::thread::spawn(system_fonts::fallback_font_faces));

    let fullscreen = args.fullscreen;
    let benchmark = args.benchmark;
    let mut style = args.style.clone().unwrap_or_default();
    let cache_db = args.cache.clone().unwrap_or_else(|| "/tmp/mbgl-cache.db".to_owned());

    if benchmark {
        Log::info(Event::General, "BENCHMARK MODE: Some optimizations are disabled.");
    }

    // Set access token if present
    let apikey = args.apikey.clone().or_else(|| std::env::var("MLN_API_KEY").ok()).unwrap_or_default();

    let map_tiler_configuration = TileServerOptions::map_tiler_configuration();
    let resource_options = ResourceOptions::default()
        .with_cache_path(cache_db)
        .with_api_key(apikey)
        .with_tile_server_options(map_tiler_configuration.clone());
    let client_options = ClientOptions::default();
    let ordered_styles = map_tiler_configuration.default_styles();

    let view = match GLFWView::new(fullscreen, benchmark) {
        Ok(view) => view,
        Err(e) => {
            Log::error(Event::GraphicsBackend, &e);
            std::process::exit(1);
        }
    };
    Log::info(Event::Render, format!("Rendering with {}", view.describe_backend()));
    if let Some(path) = &args.readback {
        view.set_readback(path.into());
    }
    if let Some(seconds) = args.quit_after {
        view.set_quit_after(seconds);
    }
    if args.gesture_test {
        view.set_gesture_test();
    }

    let manager = FileSourceManager::get();
    let online_file_source = manager.get_file_source(FileSourceType::Network, &resource_options, &client_options);
    if !settings.online {
        match &online_file_source {
            Some(online) => {
                online.set_property("online-status", &feature::Value::Bool(false));
                Log::warning(Event::Setup, "Application is offline. Press `O` to toggle online status.");
            }
            None => Log::warning(
                Event::Setup,
                "Network resource provider is not available, only local requests are supported.",
            ),
        }
    }

    let renderer_backend = view.get_renderer_backend();
    let renderer = Renderer::new(renderer_backend.clone(), view.get_pixel_ratio(), None);
    let renderer_frontend = GLFWRendererFrontend::new(renderer, renderer_backend, view.shared());
    view.set_render_frontend(renderer_frontend.clone());

    // TODO(port): ActionJournalOptions, withRenderedFeatureInfo(showFeatures).
    let mut map = Map::with_resource_options(
        &renderer_frontend,
        view.map_observer(),
        MapOptions::new().with_size(view.get_size()).with_pixel_ratio(view.get_pixel_ratio()),
        &resource_options,
        &client_options,
    )
    .unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });

    if let Err(e) = map.set_bounds(&BoundOptions::default().with_max_pitch(settings.max_pitch)) {
        Log::warning(Event::Setup, e);
    }

    if !style.is_empty() && !style.contains("://") {
        style = format!("file://{style}");
    }

    map.jump_to(
        &CameraOptions::default()
            .with_center(LatLng::new(settings.latitude, settings.longitude).ok())
            .with_center_altitude(Some(settings.altitude))
            .with_zoom(Some(settings.zoom))
            .with_bearing(Some(settings.bearing))
            .with_pitch(Some(settings.pitch))
            .with_roll(Some(settings.roll))
            .with_fov(Some(settings.fov)),
    );
    map.set_debug(MapDebugOptions(settings.debug));

    let map = Rc::new(RefCell::new(map));
    view.set_map(map.clone());

    let online = Rc::new(Cell::new(settings.online));
    {
        let online = online.clone();
        let online_file_source = online_file_source.clone();
        view.set_online_status_callback(move || {
            let Some(online_file_source) = &online_file_source else {
                Log::warning(Event::Setup, "Cannot change online status. Network resource provider is not available.");
                return;
            };
            online.set(!online.get());
            online_file_source.set_property("online-status", &feature::Value::Bool(online.get()));
            Log::info(
                Event::Setup,
                format!(
                    "Application is {}. Press `O` to toggle online status.",
                    if online.get() { "online" } else { "offline" }
                ),
            );
        });
    }

    {
        let map = map.clone();
        let shared = view.shared();
        let mut current_style_index: u8 = 0;
        view.set_change_style_callback(move || {
            current_style_index = current_style_index.wrapping_add(1);
            if usize::from(current_style_index) == ordered_styles.len() {
                current_style_index = 0;
            }
            let new_style = &ordered_styles[usize::from(current_style_index)];
            map.borrow_mut().get_style_mut().load_url(new_style.get_url());
            shared.set_window_title(new_style.get_name());
            Log::info(Event::Setup, format!("Changed style to: {}", new_style.get_name()));
        });
    }

    // Resource loader controls top-level request processing and can resume /
    // pause all managed sources simultaneously.
    let resource_loader = manager.get_file_source(FileSourceType::ResourceLoader, &resource_options, &client_options);
    {
        let mut is_paused = false;
        view.set_pause_resume_callback(move || {
            if let Some(resource_loader) = &resource_loader {
                if is_paused {
                    resource_loader.resume();
                } else {
                    resource_loader.pause();
                }
            }
            is_paused = !is_paused;
        });
    }

    view.set_reset_cache_callback(|| {
        // TODO(port): DatabaseFileSource::resetDatabase (the write path).
        Log::warning(Event::Database, "Resetting the cache database is not ported yet");
    });

    // Load style
    if style.is_empty() {
        match std::env::var("MLN_STYLE_URL") {
            Err(_) => {
                style = "https://tiles.openfreemap.org/styles/liberty".to_owned();
                view.set_window_title("OpenFreeMap Liberty");
            }
            Ok(url) => {
                view.set_window_title(&url);
                style = url;
            }
        }
    }

    if let Some(font_discovery) = font_discovery {
        let faces = font_discovery.join().unwrap_or_default();
        if faces.is_empty() {
            Log::info(Event::Setup, "No system fonts found for the scripts that glyph PBFs can't shape");
        } else {
            let families: Vec<&str> = faces.iter().map(|face| face.family.as_str()).collect();
            Log::info(Event::Setup, format!("Falling back to system fonts: {}", families.join(", ")));
        }
        map.borrow_mut().get_style_mut().set_fallback_font_face_list(system_fonts::into_font_faces(faces));
    }

    map.borrow_mut().get_style_mut().load_url(&style);

    view.run();

    // Save settings
    let camera = map.borrow().get_camera_options(None);
    if let Some(center) = camera.center {
        settings.latitude = center.latitude();
        settings.longitude = center.longitude();
    }
    settings.altitude = camera.center_altitude.unwrap_or(settings.altitude);
    settings.zoom = camera.zoom.unwrap_or(settings.zoom);
    settings.bearing = camera.bearing.unwrap_or(settings.bearing);
    settings.pitch = camera.pitch.unwrap_or(settings.pitch);
    settings.roll = camera.roll.unwrap_or(settings.roll);
    settings.fov = camera.fov.unwrap_or(settings.fov);
    // TODO(port): settings.save().
    Log::info(
        Event::General,
        format!(
            "Exit location: --lat=\"{:.6}\" --lon=\"{:.6}\" --alt=\"{:.6}\" --zoom=\"{:.6}\" --bearing=\"{:.6}\" --roll=\"{:.6}\" --fov=\"{:.6}\" --maxPitch=\"{:.6}\"",
            settings.latitude,
            settings.longitude,
            settings.altitude,
            settings.zoom,
            settings.bearing,
            settings.roll,
            settings.fov,
            settings.max_pitch
        ),
    );

    // Destruction order as in C++ (map, frontend, view): the map resets the
    // renderer, then the view drops the surface before the window.
    drop(map);
    drop(renderer_frontend);
    drop(view);
}
