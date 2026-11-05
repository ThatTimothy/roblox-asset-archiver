#[derive(Clone, Debug)]
pub enum DownloadType {
    Place,
}

pub fn download(output_dir: String, download_type: DownloadType, place_id: u64, references: bool) {
    match download_type {
        DownloadType::Place => {
            println!("Downloading place {place_id} to {output_dir} with references={references}")
        }
    }
}
