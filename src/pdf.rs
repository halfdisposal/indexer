use std::error::Error;

pub fn extract_pdf(file_path: &str) -> Result<Vec<String>, Box<dyn Error>>{
    let data = std::fs::read(file_path)?;
    let res = std::panic::catch_unwind(|| pdftext::extract_pages(&data));
    match res {
        Ok(r) => Ok(r?),
        Err(_) => Err(format!("parser panicked on {file_path}").into()),
    }
}
