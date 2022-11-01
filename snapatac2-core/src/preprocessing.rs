mod qc;
mod matrix;
mod mark_duplicates;

pub use qc::{Fragment, CellBarcode, read_tss, make_promoter_map, get_barcode_count};
pub use matrix::{
    ChromValues, ChromValuesReader, GenomeIndex, GBaseIndex, Promoters,
    read_transcripts, create_tile_matrix, create_peak_matrix, create_gene_matrix,
};
pub use mark_duplicates::FlagStat;

use crate::preprocessing::{
    mark_duplicates::{BarcodeLocation, filter_bam, group_bam_by_barcode},
    qc::{FragmentSummary, QualityControl, compute_qc_count},
};

use noodles::{bam, sam::{Header, record::data::field::Tag}};
use tempfile::Builder;
use regex::Regex;
use bed_utils::bed::{
    BEDLike, tree::{GenomeRegions, BedTree, SparseBinnedCoverage},
};
use anyhow::Result;
use anndata_rs::{anndata::AnnData, anndata_trait::{DataIO, DataPartialIO}, iterator::CsrIterator};
use indicatif::{ProgressDrawTarget, ProgressIterator, ProgressBar, style::ProgressStyle};
use flate2::{Compression, write::GzEncoder};
use polars::prelude::{NamedFrom, DataFrame, Series};
use rayon::iter::{ParallelIterator, IntoParallelIterator};
use itertools::Itertools;
use std::{path::Path, fs::File, io::{Write, BufWriter}, collections::{HashSet, HashMap}};
use either::Either;

/// Convert a BAM file to a fragment file by performing the following steps:
/// 
/// 1. Filtering: remove reads that are unmapped, not primary alignment, mapq < 30,
///    fails platform/vendor quality checks, or optical duplicate.
///    For paired-end sequencing, it also removes reads that are not properly aligned.
/// 2. Deduplicate: Sort the reads by cell barcodes and remove duplicated reads
///    for each unique cell barcode.
/// 3. Output: Convert BAM records to fragments (if paired-end) or single-end reads.
/// 
/// Note the bam file needn't be sorted or filtered.
///
/// # Arguments
///
/// * `bam_file` - File name of the BAM file.
/// * `output_file` - File name of the output fragment file.
/// * `is_paired` - Indicate whether the BAM file contain paired-end reads.
/// * `barcode_tag` - Extract barcodes from TAG fields of BAM records, e.g., `barcode_tag = "CB"`.
/// * `barcode_regex` - Extract barcodes from read names of BAM records using regular expressions.
///     Reguler expressions should contain exactly one capturing group 
///     (Parentheses group the regex between them) that matches
///     the barcodes. For example, `barcode_regex = "(..:..:..:..):\w+$"`
///     extracts `bd:69:Y6:10` from
///     `A01535:24:HW2MMDSX2:2:1359:8513:3458:bd:69:Y6:10:TGATAGGTTG`.
/// * `umi_tag` - Extract UMI from TAG fields of BAM records.
/// * `umi_regex` - Extract UMI from read names of BAM records using regular expressions.
///     See `barcode_regex` for more details.
/// * `shift_left` - Insertion site correction for the left end.
/// * `shift_right` - Insertion site correction for the right end.
/// * `chunk_size` - The size of data retained in memory when performing sorting. Larger chunk sizes
///     result in faster sorting and greater memory usage.
pub fn make_fragment_file<P1: AsRef<Path>, P2: AsRef<Path>>(
    bam_file: P1,
    output_file: P2,
    is_paired: bool,
    barcode_tag: Option<[u8; 2]>,
    barcode_regex: Option<&str>,
    umi_tag: Option<[u8; 2]>,
    umi_regex: Option<&str>,
    shift_left: i64,
    shift_right: i64,
    mapq: Option<u8>,
    chunk_size: usize,
) -> FlagStat
{
    let tmp_dir = Builder::new().tempdir_in("./")
        .expect("failed to create tmperorary directory");

    if barcode_regex.is_some() && barcode_tag.is_some() {
        panic!("Can only set barcode_tag or barcode_regex but not both");
    }
    if umi_regex.is_some() && umi_tag.is_some() {
        panic!("Can only set umi_tag or umi_regex but not both");
    }
    let barcode = match barcode_tag {
        Some(tag) => BarcodeLocation::InData(Tag::try_from(tag).unwrap()),
        None => match barcode_regex {
            Some(regex) => BarcodeLocation::Regex(Regex::new(regex).unwrap()),
            None => BarcodeLocation::InReadName,
        }
    };
    let umi = match umi_tag {
        Some(tag) => Some(BarcodeLocation::InData(Tag::try_from(tag).unwrap())),
        None => match umi_regex {
            Some(regex) => Some(BarcodeLocation::Regex(Regex::new(regex).unwrap())),
            None => None,
        }
    };
 
    let mut reader = File::open(bam_file).map(bam::Reader::new).expect("cannot open bam file");
    let header: Header = fix_header(reader.read_header().unwrap()).parse().unwrap();
    reader.read_reference_sequences().unwrap();

    let f = File::create(output_file.as_ref().clone()).expect("cannot create the output file");
    let mut output: Box<dyn Write> = if output_file.as_ref().extension().and_then(|x| x.to_str()) == Some("gz") {
        Box::new(GzEncoder::new(BufWriter::new(f), Compression::default()))
    } else {
        Box::new(BufWriter::new(f))
    };

    let mut flagstat = FlagStat::default();
    let filtered_records = filter_bam(
        reader.lazy_records().map(|x| x.unwrap()), is_paired, mapq, &mut flagstat,
    );
    group_bam_by_barcode(filtered_records, &barcode, umi.as_ref(), is_paired, tmp_dir.path().to_path_buf(), chunk_size)
        .into_fragments(&header).for_each(|rec| match rec {
            Either::Left(mut x) => {
                // TODO: use checked_add_signed.
                x.set_start((x.start() as i64 + shift_left) as u64);
                x.set_end((x.end() as i64 + shift_right) as u64);
                writeln!(output, "{}", x).unwrap();
            },
            Either::Right(x) => writeln!(output, "{}", x).unwrap(),
        });
    flagstat
}

/// This function is used to fix 10X bam headers, as the headers of 10X bam
/// files do not have the required `VN` field in the `@HD` record.
fn fix_header(header: String) -> String {
    fn fix_hd_rec(rec: String) -> String {
        if rec.starts_with("@HD") {
            let mut fields: Vec<_> = rec.split('\t').collect();
            if fields.len() == 1 || !fields[1].starts_with("VN") {
                fields.insert(1, "VN:1.0");
                fields.join("\t")
            } else {
                rec
            }
        } else {
            rec
        }
    }

    match header.split_once('\n') {
        None => fix_hd_rec(header),
        Some((line1, others)) => [&fix_hd_rec(line1.to_owned()), others].join("\n"),
    }
}

pub fn import_fragments<B, I>(
    anndata: &AnnData,
    fragments: I,
    promoter: &BedTree<bool>,
    regions: &GenomeRegions<B>,
    white_list: Option<&HashSet<String>>,
    min_num_fragment: u64,
    min_tsse: f64,
    fragment_is_sorted_by_name: bool,
    chunk_size: usize,
    ) -> Result<()>
where
    B: BEDLike + Clone + std::marker::Sync,
    I: Iterator<Item = Fragment>,
{
    let num_features = SparseBinnedCoverage::<_, u8>::new(regions, 1).len;
    let mut saved_barcodes = Vec::new();
    let mut qc = Vec::new();

    if fragment_is_sorted_by_name {
        let spinner = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr_with_hz(1)).with_style(
            ProgressStyle::with_template("{spinner} Processed {human_pos} barcodes in {elapsed} ({per_sec}) ...").unwrap()
        );
        let mut scanned_barcodes = HashSet::new();
        anndata.get_obsm().inner().insert_from_row_iter(
            "insertion",
            CsrIterator {
                iterator: fragments
                .group_by(|x| { x.barcode.clone() }).into_iter().progress_with(spinner)
                .filter(|(key, _)| white_list.map_or(true, |x| x.contains(key)))
                .chunks(chunk_size).into_iter().map(|chunk| {
                    let data: Vec<(String, Vec<Fragment>)> = chunk
                        .map(|(barcode, x)| (barcode, x.collect())).collect();
                    let result: Vec<_> = data.into_par_iter()
                        .map(|(barcode, x)| (
                            barcode,
                            compute_qc_count(x, promoter, regions, min_num_fragment, min_tsse)
                        )).collect();
                    result.into_iter().filter_map(|(barcode, r)| {
                        if !scanned_barcodes.insert(barcode.clone()) {
                            panic!("Please sort fragment file by barcodes");
                        }
                        match r {
                            None => None,
                            Some((q, count)) => {
                                saved_barcodes.push(barcode);
                                qc.push(q);
                                Some(count)
                            },
                        }
                    }).collect::<Vec<_>>()
                }),
                num_cols: num_features,
            }
        )?;
    } else {
        let spinner = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr_with_hz(1)).with_style(
            ProgressStyle::with_template("{spinner} Processed {human_pos} reads in {elapsed} ({per_sec}) ...").unwrap()
        );
        let mut scanned_barcodes = HashMap::new();
        fragments.progress_with(spinner)
        .filter(|frag| white_list.map_or(true, |x| x.contains(frag.barcode.as_str())))
        .for_each(|frag| {
            let key = frag.barcode.as_str();
            match scanned_barcodes.get_mut(key) {
                None => {
                    let mut summary= FragmentSummary::new(promoter);
                    let mut counts = SparseBinnedCoverage::new(regions, 1);
                    summary.update(&frag);
                    frag.to_insertions().into_iter().for_each(|x| counts.insert(&x, 1));
                    scanned_barcodes.insert(key.to_string(), (summary, counts));
                },
                Some((summary, counts)) => {
                    summary.update(&frag);
                    frag.to_insertions().into_iter().for_each(|x| counts.insert(&x, 1));
                }
            }
        });
        let csr_matrix: Box<dyn DataPartialIO> = Box::new(CsrIterator {
            iterator: scanned_barcodes.drain()
            .filter_map(|(barcode, (summary, binned_coverage))| {
                let q = summary.get_qc();
                if q.num_unique_fragment < min_num_fragment || q.tss_enrichment < min_tsse {
                    None
                } else {
                    saved_barcodes.push(barcode);
                    qc.push(q);
                    let count: Vec<(usize, u8)> = binned_coverage.get_coverage()
                        .iter().map(|(k, v)| (*k, *v)).collect();
                    Some(count)
                }
            }),
            num_cols: num_features,
        }.to_csr_matrix());
        anndata.get_obsm().inner().add_data("insertion", &csr_matrix)?;
    }

    let chrom_sizes: Box<dyn DataIO> = Box::new(DataFrame::new(vec![
        Series::new(
            "reference_seq_name",
            regions.regions.iter().map(|x| x.chrom()).collect::<Series>(),
        ),
        Series::new(
            "reference_seq_length",
            regions.regions.iter().map(|x| x.end()).collect::<Series>(),
        ),
    ]).unwrap());
    anndata.get_uns().inner().add_data("reference_sequences", &chrom_sizes)?;
    anndata.set_obs(Some(&qc_to_df(saved_barcodes, qc)))?;
    Ok(())
}

fn qc_to_df(
    cells: Vec<CellBarcode>,
    qc: Vec<QualityControl>,
) -> DataFrame {
    DataFrame::new(vec![
        Series::new("Cell", cells),
        Series::new(
            "tsse", 
            qc.iter().map(|x| x.tss_enrichment).collect::<Series>(),
        ),
        Series::new(
            "n_fragment", 
            qc.iter().map(|x| x.num_unique_fragment).collect::<Series>(),
        ),
        Series::new(
            "frac_dup", 
            qc.iter().map(|x| x.frac_duplicated).collect::<Series>(),
        ),
        Series::new(
            "frac_mito", 
            qc.iter().map(|x| x.frac_mitochondrial).collect::<Series>(),
        ),
    ]).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fix_header() {
        let input1 = "@HD\tVN:1.0\tSO:coordinate".to_owned();
        assert!(input1.parse::<Header>().is_ok());
        assert!(fix_header(input1.clone()).parse::<Header>().is_ok());

        let input2 = "@HD\tSO:coordinate".to_owned();
        assert_eq!(fix_header(input2), input1);

        let input3 = "@HD".to_owned();
        assert!(input3.parse::<Header>().is_err());
        assert!(fix_header(input3).parse::<Header>().is_ok());

        let input4 = "@HD\tSO:coordinate\n@SQ\tSN:chr1\tLN:195471971".to_owned();
        assert!(input4.parse::<Header>().is_err());
        assert!(fix_header(input4).parse::<Header>().is_ok());
    }
}