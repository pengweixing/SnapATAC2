use pyo3::prelude::*;
use numpy::{Ix2, PyReadonlyArray};
use std::fs::File;
use std::path::Path;
use std::io::Read;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use statrs::distribution::{Binomial, DiscreteCDF};

use snapatac2_core::{
    motif,
};

#[pyclass]
#[repr(transparent)]
#[derive(Clone)]
pub struct PyDNAMotif(pub motif::DNAMotif);

#[pymethods]
impl PyDNAMotif {
    #[new]
    fn new<'py>(
        name: &str,
        matrix: &'py PyAny,
    ) -> Self {
        let pwm: PyReadonlyArray<f64, Ix2> = matrix.extract().unwrap();
        let motif = motif::DNAMotif {
            name: name.to_string(),
            probability: pwm.as_array().rows().into_iter()
                .map(|row| row.into_iter().map(|x| *x).collect::<Vec<_>>().try_into().unwrap()).collect(),
        };
        PyDNAMotif(motif)
    }

    #[getter]
    fn name(&self) -> String { self.0.name.clone() }

    fn info_content(&self) -> f64 { self.0.info_content() }

    #[args(
        a = "0.25",
        c = "0.25",
        g = "0.25",
        t = "0.25",
    )]
    fn with_nucl_prob(&self, a: f64, c: f64, g: f64, t: f64) -> PyDNAMotifScanner {
        PyDNAMotifScanner(self.0.clone().to_scanner(motif::BackgroundProb([a, c, g, t])))
    }
}

#[pyclass]
#[repr(transparent)]
#[derive(Clone)]
pub struct PyDNAMotifScanner(pub motif::DNAMotifScanner);

#[pymethods]
impl PyDNAMotifScanner {
    #[getter]
    fn name(&self) -> String { self.0.motif.name.clone() }

    #[args(
        seq,
        pvalue = "1e-5",
    )]
    fn find(&self, seq: &str, pvalue: f64) -> Vec<(usize, f64)> {
        self.0.find(seq.as_bytes(), pvalue).collect()
    }

    #[args(
        seq,
        pvalue = "1e-5",
        rc = "true",
    )]
    fn exist(&self, seq: &str, pvalue: f64, rc: bool) -> bool {
        self.0.find(seq.as_bytes(), pvalue).next().is_some() ||
            (rc && self.0.find(rev_compl(seq).as_bytes(), pvalue).next().is_some())
    }

    #[args(
        seqs,
        pvalue = "1e-5",
        rc = "true",
    )]
    fn exists(&self, seqs: Vec<&str>, pvalue: f64, rc: bool) -> Vec<bool> {
        seqs.into_par_iter().map(|x| self.exist(x, pvalue, rc)).collect()
    }

    #[args(
        seqs,
        pvalue = "1e-5",
    )]
    fn with_background(&self, seqs: Vec<&str>, pvalue: f64) -> PyDNAMotifTest {
        let n = seqs.len();
        PyDNAMotifTest {
            scanner: self.clone(),
            pvalue,
            occurrence_background: seqs.into_par_iter().filter(|x| self.exist(x, pvalue, true)).count(),
            total_background: n,
        }
    }
}

fn rev_compl(dna: &str) -> String {
    dna.chars().rev().map(|x| match x {
        'A' | 'a' => 'T',
        'C' | 'c' => 'G',
        'G' | 'g' => 'C',
        'T' | 't' => 'A',
        c => c,
    }).collect()
}

#[pyclass]
pub struct PyDNAMotifTest {
    scanner: PyDNAMotifScanner,
    pvalue: f64,
    occurrence_background: usize,
    total_background: usize,
}

#[pymethods]
impl PyDNAMotifTest {
    #[getter]
    fn name(&self) -> String { self.scanner.name() }

    fn test(&self, seqs: Vec<&str>) -> (f64, f64) {
        let n = seqs.len().try_into().unwrap();
        let occurrence: u64 = seqs.into_par_iter()
            .filter(|x| self.scanner.exist(x, self.pvalue, true)).count()
            .try_into().unwrap();
        let p = self.occurrence_background as f64 / self.total_background as f64;
        let log_fc = ((occurrence as f64 / n as f64) / p).log2();
        let bion = Binomial::new(p, n).unwrap();
        let pval = if log_fc >= 0.0 {
            1.0 - bion.cdf(occurrence)
        } else {
            bion.cdf(occurrence)
        };
        (log_fc, pval)
    }
}

#[pyfunction]
pub(crate) fn read_motifs(
    filename: &str,
) -> Vec<PyDNAMotif> {
    let path = Path::new(filename);
    let mut file = match File::open(&path) {
        Err(why) => panic!("couldn't open file: {}", why),
        Ok(file) => file,
    };
    let mut s = String::new();
    file.read_to_string(&mut s).unwrap();
    motif::parse_meme(&s).into_iter().map(|x| PyDNAMotif(x)).collect()
}
