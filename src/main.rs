#![allow(unused)]
use crate::pdf::extract_pdf;
use std::io::Read;
use std::num;
use std::path::PathBuf;
use std::{error::Error, fs::write};
use std::collections::{HashMap, HashSet};
use serde::{Serialize, Deserialize};
use glob::glob;
use colored::Colorize;
use rayon::prelude::*;
use argp::FromArgs;

mod pdf;
mod stopwords;

#[derive(FromArgs)]
#[argp(description = "File Indexer and Searcher")]
struct Options {
    #[argp(option, short = 'i', long = "index", description = "index a directory", default = "String::from(\"N/A\")")]
    index: String,

    #[argp(option, short = 'p', long = "pattern", description = "pattern to index a directory [default = pdf]", default = "String::from(\"pdf\")")]
    pattern: String,

    #[argp(option, short = 's', long = "save", description = "save the index", default = "String::from(\"N/A\")")]
    save: String,

    #[argp(option, short = 'l', long = "load", description = "load the index", default = "String::from(\"N/A\")")]
    load: String,

    #[argp(option, short = 'q', long = "query", description = "find the best docs based on query", default = "String::from(\"N/A\")")]
    query: String,

    #[argp(option, short = 'b', long = "best", description = "find the best docs based on query", default = "3")]
    best: usize,

    #[argp(option, short = 'j', long = "jobs", description = "threads to use for indexing [default = 1]", default = "1")]
    j: usize,

    #[argp(switch, short = 'd', long = "display", description = "show all indexed files and their size")]
    display: bool,
}

mod json_as_string {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::Value;
    use std::collections::HashMap;
    pub fn serialize<S: Serializer>(
        map: &HashMap<String, f32>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let strings: HashMap<&String, String> =
            map.iter().map(|(k, v)| (k, v.to_string())).collect();
        strings.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<HashMap<String, f32>, D::Error> {
        HashMap::<String, String>::deserialize(d)?
            .into_iter()
            .map(|(k, v)| {
                serde_json::from_str(&v)
                    .map(|v| (k, v))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct Document {
    name: String,
    path: String,
    pages: usize,
    text: String,
    doctype: String,
    #[serde(with = "json_as_string")]
    tfidf: HashMap<String, f32>
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
struct Index {
    cache: Vec<Document>,
}

impl Index {
    fn new() -> Self {
        Self { cache: Vec::<Document>::new() }
    }

    fn save(&self, model_path: &str) -> std::io::Result<()> {
        if !model_path.ends_with(".bin") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid save path `{}`: expected a `.bin` extension", model_path),
            ));
        }
        let encoded: Vec<u8> = bincode::serde::encode_to_vec(self, bincode::config::standard())
            .map_err(|err| std::io::Error::other(format!("installed crated encoding failed: {err}")))
            .unwrap();
        std::fs::write(model_path, encoded)
    }

    fn load(model_path: &str) -> std::io::Result<Self> {
        let bytes = std::fs::read(model_path)?;
        let (decoded, _): (Self, _) = bincode::serde::decode_from_slice(&bytes, bincode::config::standard())
            .map_err(|err| std::io::Error::other(format!("installed crates decoding failed: {err}")))?;
        Ok(decoded)
    }

    fn process_pdf(entry: &std::path::Path) -> Result<Option<Document>, Box<dyn Error + Send + Sync>> {
        let entry_path = entry.to_string_lossy().to_string();
        let name = entry_path
            .split(['\\', '/'])
            .next_back()
            .unwrap_or("")
            .to_string();
        let pages = match extract_pdf(&entry_path).map_err(|e| e.to_string()) {
            Ok(p) => p,
            Err(e) => {
                println!("{}({}): {}", "skipping".red(), e, name.yellow());
                return Ok(None);
            }
        };

        if pages.iter().all(|p| p.trim().is_empty()) {
            println!("{} (no text): {}", "Skipping".red(), name.yellow());
            return Ok(None);
        }
        let doc = Document {
            name: name.clone(),
            path: entry_path.to_string(),
            pages: pages.len(),
            text: stopwords::remove_stopwords(pages.join(" ").as_str()),
            doctype: "pdf".to_string(),
            tfidf: HashMap::<String, f32>::new()
        };
        println!("Indexed: {} ({})", name.green(), &doc.pages.to_string().red());
        Ok(Some(doc))
    }

    fn process_text(entry: &std::path::Path, pattern: &str) -> Result<Option<Document>, Box<dyn Error + Send + Sync>> {
        let entry_path = entry.to_string_lossy().to_string();
        let name = entry_path
            .split(['\\', '/'])
            .next_back()
            .unwrap_or("")
            .to_string();
        let mut f = std::fs::File::open(&entry_path)?;
        let mut s = String::new();
        f.read_to_string(&mut s)?;

        let doc = Document {
            name: name.clone(),
            path: entry_path.to_string(),
            pages: s.len(),
            text: stopwords::remove_stopwords(&s),
            doctype: pattern.to_string(),
            tfidf: HashMap::<String, f32>::new()
        };
        println!("Indexed: {} ({})", name.green(), &doc.pages.to_string().red());
        Ok(Some(doc))
    }

    fn index(&mut self, dir_path: &str, pattern: &str, recurse: bool, num_threads: usize) -> Result<(), Box<dyn Error>> {
        if ["dat", "data", "bin", "png", "jpg", "jpeg", "svg"].contains(&pattern.to_lowercase().as_str()) {
            return Err(format!("invalid pattern `{}` for indexing", pattern.to_string().red()).into());
        }
        let max_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        if num_threads > max_cores {
            return Err(format!("available threads {} provided {}", max_cores.to_string().yellow(), num_threads.to_string().yellow()).into());
        }

        let dir = dir_path.split(['\\', '/']).collect::<Vec<_>>().join("/");
        let s = if recurse { "/**/*." } else { "/*." };
        let entries: Vec<PathBuf> = glob(&(dir.to_string() + s + pattern)).expect("Failed to find the files").flatten().collect();
        println!("Indexing {} {} files", &entries.len().to_string().red(), &pattern.yellow());
        let paths: Vec<_> = {
            let known: HashSet<&str> = self.cache.iter().map(|d| d.path.as_str()).collect();
            entries.into_iter().filter(|p| {
                let notskip = !known.contains(p.to_string_lossy().as_ref());
                if !notskip {
                    let name = p.to_string_lossy()
                        .split(['\\', '/'])
                        .next_back()
                        .unwrap_or("")
                        .to_string();
                    println!("{} (already indexed): {}", "Skipping".red(), name.yellow());
                }
                notskip
            }).collect()
        };


        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .stack_size(16 * 1024 * 1024)
            .build()?;

        let docs: Result<Vec<Option<Document>>, Box<dyn Error + Send + Sync>> = pool.install(|| {
            paths
                .par_iter()
                .map(|entry| -> Result<Option<Document>, Box<dyn Error + Send + Sync>> {
                    let entry_path = entry.to_string_lossy().to_string();
                    if pattern == "pdf" {
                        Self::process_pdf(entry)
                    } else if ["md", "txt", "text"].contains(&pattern) {
                        Self::process_text(entry, pattern)
                    } else {
                        println!("{}: indexing for files of type {} are unsupported", "error".red().bold(), &pattern.yellow());
                        Ok(None)
                    }
                })
                .collect()
        });

        let docs: Vec<Document> = docs.map_err(|e| e as Box<dyn Error>)?.into_iter().flatten().collect();
        let count = docs.len();
        self.cache.extend(docs);

        

        println!("calculating {}", "tfidf".blue());
        let idf = self.calculate_idf();
        for d in self.cache.iter_mut() {
            Self::calculate_tfidf(d, &idf);
        }
        println!("{}", "done!".green());
        println!("indexed {} documents", count.to_string().green());
        Ok(())
    }

    fn list(&self) {
        for (i, doc) in self.cache.iter().enumerate() {
            println!("{}. {}({})", i + 1, doc.name.green(), doc.pages.to_string().red());
        }
    }

    fn calculate_idf(&self) -> HashMap<String, f32> {
        let n = self.cache.len() as f32;

        let mut df: HashMap<&str, u32> = HashMap::new();
        for d in &self.cache {
            let uwords: HashSet<&str> = d.text.split(' ').filter(|w| !w.is_empty()).collect();
            for w in uwords {
                *df.entry(w).or_insert(0) += 1;
            }
        }

        df.into_iter()
            .map(|(w, f)| (w.to_string(), ((n + 1.0) / (f as f32 + 1.0)).ln()))
            .collect()
    }

    fn calculate_tfidf(doc: &mut Document, idf: &HashMap<String, f32>) {
        let words: Vec<&str> = doc.text.split([' ']).collect();
        if words.is_empty() {
            return;
        }
        let mut f = HashMap::<&str, u32>::new();
        for w in &words {
            *f.entry(*w).or_insert(0) += 1;
        }
        let n = words.len() as f32;
        for (w, c) in f {
            let tf = c as f32 / n;
            let idf_w = idf.get(w).copied().unwrap_or(0.0);
            doc.tfidf.insert(w.to_string(), tf * idf_w);
        }
    }

    fn search(&self, query: &str, first_n: usize) {
        if self.cache.is_empty() {
            println!("{}: searching an empty index", "error".red());
            return;
        }
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(str::to_lowercase)
            .collect();
        if terms.is_empty() {
            return;
        }

        let mut scores: Vec<(&str, f32)> = self.cache.iter()
            .map(|d| {
                let s: f32 = terms.iter()
                    .map(|t| d.tfidf.get(t).copied().unwrap_or(0.0))
                    .sum();
                (d.path.as_str(), s)
            })
            .filter(|&(_, s)| s > 0.0)
            .collect();
        scores.sort_by(|a, b| b.1.total_cmp(&a.1));
        println!("Search Results:");
        for (i, (path, score)) in scores.iter().take(first_n).enumerate() {
            println!("{}. {} ({})", i + 1, path.green(), score.to_string().red());
        }
    }
}

fn indexer() -> Result<(), Box<dyn Error>> {
    let options: Options = argp::parse_args_or_exit(argp::DEFAULT);
    let mut index = Index::new();
    if options.load != "N/A" {
        index = Index::load(options.load.as_str())?;
    } else {
        println!("{}: index not loaded", "warning".green());
    }
    if options.index != "N/A" {
        let patterns: Vec<&str> = options.pattern.split("|").collect();
        let start = std::time::Instant::now();
        for p in patterns {
            index.index(options.index.as_str(), p, true, options.j)?;
        }
        let duration = start.elapsed();
        let secs = duration.as_secs();
        if secs >= 60 {
            let m = secs / 60;
            let s = secs % 60;
            println!("time for indexing: {} {}", (m.to_string() + "m").green(), (s.to_string() + "s").green());
        } else {
            let ms = duration.as_millis();
            println!("time for indexing: {}", (ms.to_string() + "ms").green());
        }
    }
    if options.save != "N/A" {
        index.save(options.save.as_str())?;
        println!("index saved as `{}`", options.save.yellow());
    }
    if options.query != "N/A" {
        index.search(options.query.as_str(), options.best);
    }
    if options.display {
        index.list();
    }
    Ok(())
}

fn main() {
    if let Err(e) = indexer() {
        eprintln!("{}: {}", "error".red().bold(), e);
        std::process::exit(1);
    }
}
