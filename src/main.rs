use std::collections::HashMap;

use bindle_writer::BindleWriter;
use expander::{ExpansionContext, InvoiceVersioning};
use hippofacts::{HippoFacts, HippoFactsEntry};

mod bindle_pusher;
mod bindle_utils;
mod bindle_writer;
mod expander;
mod hippo_notifier;
mod hippofacts;

const ARG_HIPPOFACTS: &str = "hippofacts_path";
const ARG_STAGING_DIR: &str = "output_dir";
const ARG_OUTPUT: &str = "output_format";
const ARG_VERSIONING: &str = "versioning";
const ARG_BINDLE_URL: &str = "bindle_server";
const ARG_HIPPO_URL: &str = "hippo_url";
const ARG_HIPPO_USERNAME: &str = "hippo_username";
const ARG_HIPPO_PASSWORD: &str = "hippo_password";
const ARG_ACTION: &str = "action";
const ARG_INSECURE: &str = "insecure";

const ACTION_ALL: &str = "all";
const ACTION_BINDLE: &str = "bindle";
const ACTION_PREPARE: &str = "prepare";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = clap::App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .author("Deis Labs")
        .about("Expands Hippo artifacts files for upload to application storage")
        .arg(
            clap::Arg::new(ARG_HIPPOFACTS)
                .required(true)
                .index(1)
                .about("The artifacts spec (file or directory containing HIPPOFACTS file)"),
        )
        .arg(
            clap::Arg::new(ARG_STAGING_DIR)
                .required_if_eq(ARG_ACTION, ACTION_PREPARE)
                .takes_value(true)
                .short('d')
                .long("dir")
                .about("The path to output the artifacts to (required with --action prepare, otherwise uses a temporary directory)"),
        )
        .arg(
            clap::Arg::new(ARG_VERSIONING)
                .possible_values(&["dev", "production"])
                .default_value("dev")
                .required(false)
                .short('v')
                .long("invoice-version")
                .about("How to version the generated invoice"),
        )
        .arg(
            clap::Arg::new(ARG_OUTPUT)
                .possible_values(&["id", "message", "none"])
                .default_value("message")
                .required(false)
                .short('o')
                .long("output")
                .about("What to print on success"),
        )
        .arg(
            clap::Arg::new(ARG_BINDLE_URL)
                .required_if_eq_any(&[(ARG_ACTION, ACTION_ALL), (ARG_ACTION, ACTION_BINDLE)])
                .short('s')
                .long("server")
                .env("BINDLE_URL")
                .about("The Bindle server to push the artifacts to")
        )
        .arg(
            clap::Arg::new(ARG_HIPPO_URL)
                .required_if_eq(ARG_ACTION, ACTION_ALL)
                .long("hippo-url")
                .env("HIPPO_URL")
                .about("The Hippo service to push the artifacts to")
        )
        .arg(
            clap::Arg::new(ARG_HIPPO_USERNAME)
                .required_if_eq(ARG_ACTION, ACTION_ALL)
                .long("hippo-username")
                .env("HIPPO_USERNAME")
                .about("The username for connecting to Hippo")
        )
        .arg(
            clap::Arg::new(ARG_HIPPO_PASSWORD)
                .required_if_eq(ARG_ACTION, ACTION_ALL)
                .long("hippo-password")
                .env("HIPPO_PASSWORD")
                .about("The username for connecting to Hippo")
        )
        .arg(
            clap::Arg::new(ARG_ACTION)
                .possible_values(&[ACTION_ALL, ACTION_BINDLE, ACTION_PREPARE])
                .default_value(ACTION_ALL)
                .required(false)
                .short('a')
                .long("action")
                .about("What action to take with the generated bindle"),
        )
        .arg(
            clap::Arg::new(ARG_INSECURE)
                .required(false)
                .takes_value(false)
                .short('k')
                .long("insecure")
                .about("If set, ignore server certificate errors"),
        )
        .get_matches();

    let hippofacts_arg = args
        .value_of(ARG_HIPPOFACTS)
        .ok_or_else(|| anyhow::Error::msg("HIPPOFACTS file is required"))?;
    let staging_dir_arg = args.value_of(ARG_STAGING_DIR);
    let versioning_arg = args.value_of(ARG_VERSIONING).unwrap();
    let output_format_arg = args.value_of(ARG_OUTPUT).unwrap();
    let bindle_url = args.value_of(ARG_BINDLE_URL).map(|s| s.to_owned());
    let bindle_settings = match args.value_of(ARG_ACTION) {
        None | Some(ACTION_PREPARE) => BindleSettings::NoPush(bindle_url),
        _ => BindleSettings::Push(bindle_url.ok_or_else(|| anyhow::anyhow!("Bindle URL must be set for this action"))?),
    };
    let hippo_url = match args.value_of(ARG_ACTION) {
        Some(ACTION_ALL) => args.value_of(ARG_HIPPO_URL).map(|s| s.to_owned()),
        _ => None,
    };
    let hippo_username = args.value_of(ARG_HIPPO_USERNAME);
    let hippo_password = args.value_of(ARG_HIPPO_PASSWORD);

    let notify_to = hippo_url.map(|url| hippo_notifier::ConnectionInfo {
        url,
        danger_accept_invalid_certs: args.is_present(ARG_INSECURE),
        username: hippo_username.unwrap().to_owned(), // Known to be set if the URL is
        password: hippo_password.unwrap().to_owned(),
    });

    let source_file_or_dir = std::env::current_dir()?.join(hippofacts_arg);
    let source = if source_file_or_dir.is_file() {
        source_file_or_dir
    } else {
        source_file_or_dir.join("HIPPOFACTS")
    };
    if !source.exists() {
        return Err(anyhow::anyhow!(
            "Artifacts spec not found: file {} does not exist",
            source.to_string_lossy()
        ));
    }

    let destination = match staging_dir_arg {
        Some(dir) => std::env::current_dir()?.join(dir),
        None => std::env::temp_dir().join("hippo-staging"), // TODO: make unpredictable?
    };
    let invoice_versioning = InvoiceVersioning::parse(versioning_arg);
    let output_format = OutputFormat::parse(output_format_arg);

    run(
        &source,
        &destination,
        invoice_versioning,
        output_format,
        bindle_settings,
        notify_to,
    )
    .await
}

async fn run(
    source: impl AsRef<std::path::Path>,
    destination: impl AsRef<std::path::Path>,
    invoice_versioning: InvoiceVersioning,
    output_format: OutputFormat,
    bindle_settings: BindleSettings,
    notify_to: Option<hippo_notifier::ConnectionInfo>,
) -> anyhow::Result<()> {
    let spec = HippoFacts::read_from(&source)?;

    let source_dir = source
        .as_ref()
        .parent()
        .ok_or_else(|| anyhow::Error::msg("Can't establish source directory"))?
        .to_path_buf();

    // Do this outside the `expand` function so `expand` is more testable
    let external_invoices = prefetch_required_invoices(&spec, bindle_settings.bindle_url()).await?;

    let expansion_context = ExpansionContext {
        relative_to: source_dir.clone(),
        invoice_versioning,
        external_invoices,
    };

    let invoice = expander::expand(&spec, &expansion_context)?;

    let writer = BindleWriter::new(&source_dir, &destination);
    writer.write(&invoice).await?;

    if let BindleSettings::Push(url) = &&bindle_settings {
        bindle_pusher::push_all(&destination, &invoice.bindle.id, &url).await?;
        if let Some(hippo_url) = &notify_to {
            hippo_notifier::register(&invoice.bindle.id, &hippo_url).await?;
        }
    }

    // TODO: handle case where push succeeded but notify failed
    match output_format {
        OutputFormat::None => (),
        OutputFormat::Id => println!("{}", &invoice.bindle.id),
        OutputFormat::Message => match &bindle_settings {
            BindleSettings::Push(_) =>
                println!("pushed: {}", &invoice.bindle.id),
            BindleSettings::NoPush(_) => {
                println!("id:      {}", &invoice.bindle.id);
                println!(
                    "command: bindle push -p {} {}",
                    dunce::canonicalize(&destination)?.to_string_lossy(),
                    &invoice.bindle.id
                );
            },
        }
    }

    Ok(())
}

async fn prefetch_required_invoices(
    hippofacts: &HippoFacts,
    bindle_url: Option<String>,
) -> anyhow::Result<HashMap<bindle::Id, bindle::Invoice>> {
    let mut map = HashMap::new();

    let external_refs: Vec<bindle::Id> = hippofacts
        .entries
        .iter()
        .flat_map(external_bindle_id)
        .collect();
    if external_refs.is_empty() {
        return Ok(map);
    }

    let base_url = bindle_url.as_ref().ok_or_else(|| {
        anyhow::anyhow!("Spec file contains external references but Bindle server URL is not set")
    })?;
    let client = bindle::client::Client::new(base_url)?;

    for external_ref in external_refs {
        let invoice = client.get_yanked_invoice(&external_ref).await?;
        map.insert(external_ref, invoice);
    }

    Ok(map)
}

fn external_bindle_id(entry: &HippoFactsEntry) -> Option<bindle::Id> {
    entry.external_ref().map(|ext| ext.bindle_id.clone())
}

enum OutputFormat {
    None,
    Id,
    Message,
}

impl OutputFormat {
    pub fn parse(text: &str) -> Self {
        if text == "none" {
            OutputFormat::None
        } else if text == "id" {
            OutputFormat::Id
        } else {
            OutputFormat::Message
        }
    }
}

enum BindleSettings {
    NoPush(Option<String>),
    Push(String),
}

impl BindleSettings {
    pub fn bindle_url(&self) -> Option<String> {
        match self {
            Self::NoPush(opt) => opt.clone(),
            Self::Push(url) => Some(url.clone()),
        }
    }
}
