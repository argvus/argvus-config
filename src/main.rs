use argvus_config_core::{ConfigDocument, ConfigScope, ConfigStore};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::process::ExitCode;

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("argvus-config: {error}");
            ExitCode::from(1)
        }
    }
}

fn run(arguments: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    let store = ConfigStore::from_environment()?;
    let command = arguments.first().map(String::as_str).unwrap_or("help");
    match command {
        "path" => println!("{}", store.path.display()),
        "validate" => {
            store.load()?.validate()?;
            println!("valid");
        }
        "ensure" | "init" => {
            store.ensure()?;
            println!("ready");
        }
        "migrate" => {
            store.migrate_legacy()?;
            println!("migrated");
        }
        "get" => get_value(&store, &arguments[1..])?,
        "set" => set_value(&store, &arguments[1..])?,
        "unset" => {
            let pointer = arguments.get(1).ok_or("usage: argvus-config unset /path")?;
            store.unset(pointer)?;
        }
        "export" => export_scope(&store, &arguments[1..])?,
        "import" => import_scope(&store, &arguments[1..])?,
        "project" => project_config(
            &store,
            arguments.iter().any(|argument| argument == "--force"),
        )?,
        "help" | "--help" | "-h" => print_help(),
        other => return Err(format!("unknown command: {other}").into()),
    }
    Ok(())
}

fn project_config(store: &ConfigStore, force: bool) -> Result<(), Box<dyn std::error::Error>> {
    let system_config = env::var_os("ARGVUS_SYSTEM_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "/usr/share/argvus".into());
    let script = system_config.join("session/sh/project-config.sh");
    if !script.is_file() {
        return Err(format!("projection helper not found: {}", script.display()).into());
    }
    let config_home = store
        .path
        .parent()
        .and_then(Path::parent)
        .ok_or("invalid canonical configuration path")?;
    let mut command = Command::new("sh");
    command
        .arg(&script)
        .env("ARGVUS_CONFIG_HOME", config_home)
        .env("ARGVUS_SYSTEM_CONFIG", &system_config);
    if force {
        command.env("ARGVUS_PROJECT_FORCE", "1");
    }
    let status = command.status()?;
    if !status.success() {
        return Err(format!("projection helper exited with {status}").into());
    }
    Ok(())
}

fn get_value(store: &ConfigStore, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let pointer = arguments
        .first()
        .ok_or("usage: argvus-config get /path [--effective]")?;
    let effective = arguments.iter().any(|argument| argument == "--effective");
    let document = if effective {
        store.load_effective()?
    } else {
        store.load()?
    };
    let value = document.get(pointer).cloned().unwrap_or(Value::Null);
    if arguments.iter().any(|argument| argument == "--raw") {
        match value {
            Value::String(value) => println!("{value}"),
            other => println!("{other}"),
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

fn set_value(store: &ConfigStore, arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let pointer = arguments
        .first()
        .ok_or("usage: argvus-config set /path <json-value>")?;
    let raw_value = arguments
        .get(1)
        .ok_or("usage: argvus-config set /path <json-value>")?;
    let value =
        serde_json::from_str(raw_value).unwrap_or_else(|_| Value::String(raw_value.clone()));
    store.update(pointer, value)?;
    Ok(())
}

fn export_scope(
    store: &ConfigStore,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let scope_name = arguments
        .iter()
        .position(|argument| argument == "--scope")
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str)
        .unwrap_or("desktop");
    let scope = ConfigScope::parse(scope_name).ok_or("invalid scope")?;
    let document = store.load()?.scoped(scope);
    let output = arguments
        .iter()
        .position(|argument| argument == "--output")
        .and_then(|index| arguments.get(index + 1));
    let bytes = serde_json::to_vec_pretty(&document)?;
    if let Some(output) = output {
        fs::write(
            Path::new(output),
            format!("{}\n", String::from_utf8(bytes)?),
        )?;
    } else {
        println!("{}", String::from_utf8(bytes)?);
    }
    Ok(())
}

fn import_scope(
    store: &ConfigStore,
    arguments: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let scope_name = arguments
        .iter()
        .position(|argument| argument == "--scope")
        .and_then(|index| arguments.get(index + 1))
        .map(String::as_str)
        .unwrap_or("desktop");
    let scope = ConfigScope::parse(scope_name).ok_or("invalid scope")?;
    let input = arguments
        .iter()
        .rev()
        .find(|argument| !argument.starts_with('-'))
        .ok_or("missing profile")?;
    let imported: ConfigDocument = serde_json::from_str(&fs::read_to_string(input)?)?;
    imported.validate()?;
    store.modify(|current| {
        for section in scope.sections() {
            if let Some(value) = imported.sections.get(*section) {
                current
                    .sections
                    .insert((*section).to_owned(), value.clone());
            }
        }
        current.validate()
    })?;
    Ok(())
}

fn print_help() {
    println!(
        "argvus-config path|validate|ensure|migrate|get|set|unset|export|import|project [--force]"
    );
    println!("  ensure  materialize canonical defaults without resetting explicit values");
    println!("  get /appearance/theme [--effective] [--raw]");
    println!("  set /appearance/theme \"argvus-dark\"");
    println!("  export --scope appearance|desktop [--output FILE]");
    println!("  import --scope appearance|desktop FILE");
    println!("  project  incrementally generate native and generated files from config.json");
    println!("           use --force to regenerate every projection");
}
