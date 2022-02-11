use std::collections::BTreeMap;
use std::fs::File;
use std::io::{stdin, BufRead, BufReader, Cursor, Read, Write};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Instant;

use byte_unit::Byte;
use eyre::Result;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use milli::update::UpdateIndexingStep::{
    ComputeIdsAndMergeDocuments, IndexDocuments, MergeDataIntoFinalDatabase, RemapDocumentAddition,
};
use milli::update::{IndexDocumentsConfig, IndexDocumentsMethod, IndexerConfig};
use milli::Index;
use serde_json::{Map, Value};
use structopt::StructOpt;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[derive(Debug, StructOpt)]
#[structopt(name = "Milli CLI", about = "A simple CLI to manipulate a milli index.")]
struct Cli {
    #[structopt(short, long, default_value = ".")]
    index_path: PathBuf,
    #[structopt(short = "s", long, default_value = "100GiB")]
    index_size: Byte,
    /// Verbose mode (-v, -vv, -vvv, etc.)
    #[structopt(short, long, parse(from_occurrences))]
    verbose: usize,
    #[structopt(subcommand)]
    subcommand: Command,
}

#[derive(Debug, StructOpt)]
enum Command {
    Documents {
        #[structopt(subcommand)]
        cmd: Documents,
    },
    Search(Search),
    Settings {
        #[structopt(subcommand)]
        cmd: Settings,
    },
}

impl Performer for Command {
    fn perform(self, index: Index) -> Result<()> {
        match self {
            Command::Documents { cmd } => cmd.perform(index),
            Command::Search(cmd) => cmd.perform(index),
            Command::Settings { cmd } => cmd.perform(index),
        }
    }
}

#[derive(Debug, StructOpt)]
enum Settings {
    Update(SettingsUpdate),
    Show,
    User {
        #[structopt(long)]
        name: String,
        #[structopt(long)]
        ids: Vec<String>,
    },
}

impl Settings {
    fn add_user_docids(index: &Index, name: String, ids: Vec<String>) -> Result<()> {
        let mut txn = index.write_txn()?;
        index.user_add_document_filter(&mut txn, &name, ids)?;
        txn.commit()?;
        Ok(())
    }

    fn show(&self, index: Index) -> Result<()> {
        let txn = index.read_txn()?;
        let displayed_attributes = index
            .displayed_fields(&txn)?
            .map(|fields| fields.into_iter().map(String::from).collect());

        let searchable_attributes: Option<Vec<_>> = index
            .searchable_fields(&txn)?
            .map(|fields| fields.into_iter().map(String::from).collect());

        let filterable_attributes: Vec<_> = index.filterable_fields(&txn)?.into_iter().collect();

        let sortable_attributes: Vec<_> = index.sortable_fields(&txn)?.into_iter().collect();

        let criteria: Vec<_> = index.criteria(&txn)?.into_iter().map(|c| c.to_string()).collect();

        let stop_words = index
            .stop_words(&txn)?
            .map(|stop_words| -> Result<Vec<_>> {
                Ok(stop_words.stream().into_strs()?.into_iter().collect())
            })
            .transpose()?
            .unwrap_or_else(Vec::new);
        let distinct_field = index.distinct_field(&txn)?.map(String::from);

        // in milli each word in the synonyms map were split on their separator. Since we lost
        // this information we are going to put space between words.
        let synonyms: BTreeMap<_, Vec<_>> = index
            .synonyms(&txn)?
            .iter()
            .map(|(key, values)| {
                (key.join(" "), values.iter().map(|value| value.join(" ")).collect())
            })
            .collect();

        println!(
            "displayed attributes:\n\t{}\nsearchable attributes:\n\t{}\nfilterable attributes:\n\t{}\nsortable attributes:\n\t{}\ncriterion:\n\t{}\nstop words:\n\t{}\ndistinct fields:\n\t{}\nsynonyms:\n\t{}\n",
            displayed_attributes.unwrap_or(vec!["*".to_owned()]).join("\n\t"),
            searchable_attributes.unwrap_or(vec!["*".to_owned()]).join("\n\t"),
            filterable_attributes.join("\n\t"),
            sortable_attributes.join("\n\t"),
            criteria.join("\n\t"),
            stop_words.join("\n\t"),
            distinct_field.unwrap_or_default(),
            synonyms.into_iter().map(|(k, v)| format!("\n\t{}:\n{:?}", k, v)).collect::<String>(),
        );
        Ok(())
    }
}

impl Performer for Settings {
    fn perform(self, index: Index) -> Result<()> {
        match self {
            Settings::Update(update) => update.perform(index),
            Settings::Show => self.show(index),
            Settings::User { name, ids } => Self::add_user_docids(&index, name, ids),
        }
    }
}

#[derive(Debug, StructOpt)]
enum Documents {
    Add(DocumentAddition),
}

impl Performer for Documents {
    fn perform(self, index: Index) -> Result<()> {
        match self {
            Self::Add(addition) => addition.perform(index),
        }
    }
}

trait Performer {
    fn perform(self, index: Index) -> Result<()>;
}

fn setup(opt: &Cli) -> Result<()> {
    color_eyre::install()?;
    stderrlog::new()
        .verbosity(opt.verbose)
        .show_level(false)
        .timestamp(stderrlog::Timestamp::Off)
        .init()?;
    Ok(())
}

fn main() -> Result<()> {
    let command = Cli::from_args();

    setup(&command)?;

    let mut options = heed::EnvOpenOptions::new();
    options.map_size(command.index_size.get_bytes() as usize);
    let index = milli::Index::new(options, command.index_path)?;

    command.subcommand.perform(index)?;

    Ok(())
}

#[derive(Debug)]
enum DocumentAdditionFormat {
    Csv,
    Json,
    Jsonl,
}

impl FromStr for DocumentAdditionFormat {
    type Err = eyre::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "csv" => Ok(Self::Csv),
            "jsonl" => Ok(Self::Jsonl),
            "json" => Ok(Self::Json),
            other => eyre::bail!("invalid format: {}", other),
        }
    }
}

#[derive(Debug, StructOpt)]
struct DocumentAddition {
    #[structopt(short, long, default_value = "json", possible_values = &["csv", "jsonl", "json"])]
    format: DocumentAdditionFormat,
    /// Path to the update file, if not present, will read from stdin.
    #[structopt(short, long)]
    path: Option<PathBuf>,
    /// Whether to generate missing document ids.
    #[structopt(short, long)]
    autogen_docids: bool,
    /// Whether to update or replace the documents if they already exist.
    #[structopt(short, long)]
    update_documents: bool,
}

impl Performer for DocumentAddition {
    fn perform(self, index: milli::Index) -> Result<()> {
        let reader: Box<dyn Read> = match self.path {
            Some(ref path) => {
                let file = File::open(path)?;
                Box::new(file)
            }
            None => Box::new(stdin()),
        };

        println!("parsing documents...");

        let documents = match self.format {
            DocumentAdditionFormat::Csv => documents_from_csv(reader)?,
            DocumentAdditionFormat::Json => documents_from_json(reader)?,
            DocumentAdditionFormat::Jsonl => documents_from_jsonl(reader)?,
        };

        let reader = milli::documents::DocumentBatchReader::from_reader(Cursor::new(documents))?;

        println!("Adding {} documents to the index.", reader.len());

        let mut txn = index.env.write_txn()?;
        let config = milli::update::IndexerConfig { log_every_n: Some(100), ..Default::default() };
        let update_method = if self.update_documents {
            IndexDocumentsMethod::UpdateDocuments
        } else {
            IndexDocumentsMethod::ReplaceDocuments
        };

        let indexing_config = IndexDocumentsConfig {
            update_method,
            autogenerate_docids: self.autogen_docids,
            ..Default::default()
        };
        let mut bars = Vec::new();
        let progesses = MultiProgress::new();
        for _ in 0..4 {
            let bar = ProgressBar::hidden();
            let bar = progesses.add(bar);
            bars.push(bar);
        }
        let mut addition = milli::update::IndexDocuments::new(
            &mut txn,
            &index,
            &config,
            indexing_config,
            |step| indexing_callback(step, &bars),
        );
        addition.add_documents(reader)?;

        std::thread::spawn(move || {
            progesses.join().unwrap();
        });

        let result = addition.execute()?;

        txn.commit()?;

        println!("{:?}", result);
        Ok(())
    }
}

fn indexing_callback(step: milli::update::UpdateIndexingStep, bars: &[ProgressBar]) {
    let step_index = step.step();
    let bar = &bars[step_index];
    if step_index > 0 {
        let prev = &bars[step_index - 1];
        if !prev.is_finished() {
            prev.disable_steady_tick();
            prev.finish_at_current_pos();
        }
    }

    let style = ProgressStyle::default_bar()
        .template("[eta: {eta_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg}")
        .progress_chars("##-");

    match step {
        RemapDocumentAddition { documents_seen } => {
            bar.set_style(ProgressStyle::default_spinner());
            bar.set_message(format!("remaped {} documents so far.", documents_seen));
        }
        ComputeIdsAndMergeDocuments { documents_seen, total_documents } => {
            bar.set_style(style);
            bar.set_length(total_documents as u64);
            bar.set_message("Merging documents...");
            bar.set_position(documents_seen as u64);
        }
        IndexDocuments { documents_seen, total_documents } => {
            bar.set_style(style);
            bar.set_length(total_documents as u64);
            bar.set_message("Indexing documents...");
            bar.set_position(documents_seen as u64);
        }
        MergeDataIntoFinalDatabase { databases_seen, total_databases } => {
            bar.set_style(style);
            bar.set_length(total_databases as u64);
            bar.set_message("Merging databases...");
            bar.set_position(databases_seen as u64);
        }
    }
    bar.enable_steady_tick(200);
}

fn documents_from_jsonl(reader: impl Read) -> Result<Vec<u8>> {
    let mut writer = Cursor::new(Vec::new());
    let mut documents = milli::documents::DocumentBatchBuilder::new(&mut writer)?;

    let mut buf = String::new();
    let mut reader = BufReader::new(reader);

    while reader.read_line(&mut buf)? > 0 {
        documents.extend_from_json(&mut buf.as_bytes())?;
    }
    documents.finish()?;

    Ok(writer.into_inner())
}

fn documents_from_json(reader: impl Read) -> Result<Vec<u8>> {
    let mut writer = Cursor::new(Vec::new());
    let mut documents = milli::documents::DocumentBatchBuilder::new(&mut writer)?;

    documents.extend_from_json(reader)?;
    documents.finish()?;

    Ok(writer.into_inner())
}

fn documents_from_csv(reader: impl Read) -> Result<Vec<u8>> {
    let mut writer = Cursor::new(Vec::new());
    milli::documents::DocumentBatchBuilder::from_csv(reader, &mut writer)?.finish()?;

    Ok(writer.into_inner())
}

#[derive(Debug, StructOpt)]
struct Search {
    query: Option<String>,
    #[structopt(short, long)]
    filter: Option<String>,
    #[structopt(short, long)]
    offset: Option<usize>,
    #[structopt(short, long)]
    limit: Option<usize>,
    #[structopt(short, long, conflicts_with = "query")]
    interactive: bool,
    #[structopt(short, long)]
    user: Option<String>,
}

impl Performer for Search {
    fn perform(self, index: milli::Index) -> Result<()> {
        if self.interactive {
            let stdin = std::io::stdin();
            let mut lines = stdin.lock().lines();
            loop {
                eprint!("> ");
                std::io::stdout().flush()?;
                match lines.next() {
                    Some(Ok(line)) => {
                        let now = Instant::now();
                        let jsons = self.perform_single_search(&index, &Some(line))?;

                        let time = now.elapsed();

                        let hits = serde_json::to_string_pretty(&jsons)?;

                        println!("{}", hits);
                        eprintln!("found {} results in {:.02?}", jsons.len(), time);
                    }
                    _ => break,
                }
            }
        } else {
            let now = Instant::now();
            let jsons = self.perform_single_search(&index, &self.query)?;

            let time = now.elapsed();

            let hits = serde_json::to_string_pretty(&jsons)?;

            println!("{}", hits);
            eprintln!("found {} results in {:.02?}", jsons.len(), time);
        }

        Ok(())
    }
}

impl Search {
    fn perform_single_search(
        &self,
        index: &milli::Index,
        query: &Option<String>,
    ) -> Result<Vec<Map<String, Value>>> {
        let txn = index.env.read_txn()?;
        let mut search = index.search(&txn);

        if let Some(ref query) = query {
            search.query(query);
        }

        if let Some(ref filter) = self.filter {
            if let Some(condition) = milli::Filter::from_str(filter)? {
                search.filter(condition);
            }
        }

        if let Some(offset) = self.offset {
            search.offset(offset);
        }

        if let Some(limit) = self.limit {
            search.limit(limit);
        }

        if let Some(ref user) = self.user {
            search.with_user(user.to_string());
        }

        let result = search.execute()?;

        let fields_ids_map = index.fields_ids_map(&txn)?;
        let displayed_fields =
            index.displayed_fields_ids(&txn)?.unwrap_or_else(|| fields_ids_map.ids().collect());
        let documents = index.documents(&txn, result.documents_ids)?;
        let mut jsons = Vec::new();
        for (_, obkv) in documents {
            let json = milli::obkv_to_json(&displayed_fields, &fields_ids_map, obkv)?;
            jsons.push(json);
        }

        Ok(jsons)
    }
}

#[derive(Debug, StructOpt)]
struct SettingsUpdate {
    #[structopt(long)]
    filterable_attributes: Option<Vec<String>>,
    #[structopt(long)]
    criteria: Option<Vec<String>>,
}

impl Performer for SettingsUpdate {
    fn perform(self, index: milli::Index) -> Result<()> {
        let mut txn = index.env.write_txn()?;

        let config = IndexerConfig { log_every_n: Some(100), ..Default::default() };

        let mut update = milli::update::Settings::new(&mut txn, &index, &config);

        if let Some(ref filterable_attributes) = self.filterable_attributes {
            if !filterable_attributes.is_empty() {
                update.set_filterable_fields(filterable_attributes.iter().cloned().collect());
            } else {
                update.reset_filterable_fields();
            }
        }

        if let Some(criteria) = self.criteria {
            if !criteria.is_empty() {
                update.set_criteria(criteria);
            } else {
                update.reset_criteria();
            }
        }

        let mut bars = Vec::new();
        let progesses = MultiProgress::new();
        for _ in 0..4 {
            let bar = ProgressBar::hidden();
            let bar = progesses.add(bar);
            bars.push(bar);
        }

        std::thread::spawn(move || {
            progesses.join().unwrap();
        });

        update.execute(|step| indexing_callback(step, &bars))?;

        txn.commit()?;
        Ok(())
    }
}
