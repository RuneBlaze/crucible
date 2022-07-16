use ahash::AHashSet;
use fixedbitset::FixedBitSet;
use itertools::Itertools;
use ndarray::{Array, ShapeBuilder, Ix2, Axis, Slice};
use ogcat::ogtree::*;
use seq_io::fasta::{Reader, Record};
use serde::{Serialize, Deserialize};
use tracing::info;
use std::{collections::BinaryHeap, path::{PathBuf, Path}, fs::{create_dir_all, File}, io::{BufWriter, Write}};

pub struct TaxaHierarchy {
    pub reordered_taxa: Vec<usize>,
    pub decomposition_ranges: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CrucibleCtxt {
    pub nchars_partial_sum : Array::<u32, Ix2>,
    pub hmm_ranges : Vec<(usize, usize)>,
}

impl CrucibleCtxt {
    pub fn new(nchars_partial_sum : Array::<u32, Ix2>, hmm_ranges : Vec<(usize, usize)>) -> Self {
        Self {
            nchars_partial_sum,
            hmm_ranges,
        }
    }

    pub fn retrieve_nchars_noalloc(&self, hmm_idx : usize, buf : &mut [u32]) {
        let shape = self.nchars_partial_sum.shape();
        let (start, end) = self.hmm_ranges[hmm_idx];
        let k = shape[1];
        for i in 0..k {
            buf[i] = self.nchars_partial_sum[(end, i)] - self.nchars_partial_sum[(start, i)];
        }
    }

    pub fn retrieve_nchars(&self, hmm_idx : usize) -> Vec<u32> {
        let k = self.nchars_partial_sum.shape()[1];
        let mut buf = vec![0; k];
        self.retrieve_nchars_noalloc(hmm_idx, &mut buf);
        return buf;
    }

    pub fn num_hmms(&self) -> usize {
        self.hmm_ranges.len()
    }
}

pub fn hierarchical_decomp(tree: &Tree, max_size: usize) -> TaxaHierarchy {
    let n = tree.ntaxa;
    let mut reordered_taxa = (0..n).collect::<Vec<_>>();
    let mut taxa_label = FixedBitSet::with_capacity(n); // Taxa ID -> is on the left
    let mut pq = BinaryHeap::new();
    let mut cuts = AHashSet::new();
    let mut decomposition_ranges: Vec<(usize, usize)> = Vec::new();
    cuts.insert(0usize);
    pq.push((tree.ntaxa, (0usize, tree.ntaxa), 0usize));
    let mut tree_sizes = vec![0u64; tree.taxa.len()];
    for i in tree.postorder() {
        if tree.is_leaf(i) {
            tree_sizes[i] = 1;
        } else {
            tree.children(i).for_each(|c| {
                tree_sizes[i] += tree_sizes[c];
            });
        }
    }
    while let Some((size, (lb, ub), root)) = pq.pop() {
        if size >= 2 {
            decomposition_ranges.push((lb, ub));
        }
        if size < max_size {
            continue;
        }
        let it = PostorderIterator::from_node_excluding(tree, root, &cuts);
        let mut best_inbalance = u64::MAX;
        let mut best_cut = 0usize;
        let mut non_leaf = false;
        for i in it {
            if i == root {
                continue;
            }
            if tree.is_leaf(i) {
            } else {
                non_leaf = true;
                let inbalance = (size as u64 - tree_sizes[i]).abs_diff(tree_sizes[i]);
                if inbalance < best_inbalance {
                    best_inbalance = inbalance;
                    best_cut = i;
                }
            }
        } // finding the best cut
        if non_leaf {
            assert_ne!(best_inbalance, u64::MAX, "No cut found");
        } else {
            break;
        }
        for a in tree.ancestors(best_cut) {
            if a == root {
                break;
            }
            tree_sizes[a] -= tree_sizes[best_cut];
        }
        cuts.insert(best_cut);
        for u in tree.postorder_from(best_cut) {
            if tree.is_leaf(u) {
                let tid = tree.taxa[u] as usize;
                taxa_label.set(tid, true);
            }
        }
        let view = &mut reordered_taxa[lb..ub];
        view.sort_unstable_by_key(|e| !taxa_label[*e]);
        taxa_label.clear();
        pq.push((tree_sizes[best_cut] as usize, (lb, lb + tree_sizes[best_cut] as usize), best_cut));
        pq.push((size - tree_sizes[best_cut] as usize, (lb + tree_sizes[best_cut] as usize, ub), root));
    }
    TaxaHierarchy {
        reordered_taxa,
        decomposition_ranges,
    }
}

pub fn oneshot_melt(
    input: &PathBuf,
    tree: &PathBuf,
    max_size: usize,
    outdir: &PathBuf,
) -> anyhow::Result<()> {
    let collection = TreeCollection::from_newick(tree).expect("Failed to read tree");
    let decomp = hierarchical_decomp(&collection.trees[0], max_size);
    info!(num_subsets = decomp.decomposition_ranges.len(), "decomposed input tree");
    let mut reader = Reader::from_path(input)?;
    let mut records_failable: Result<Vec<_>, _> =
        reader.records().into_iter().into_iter().collect();
    let records = records_failable.as_mut().unwrap();
    let ts = &collection.taxon_set;
    records.sort_unstable_by_key(|r| {
        let taxon_name = String::from_utf8(r.head.clone()).unwrap();
        let id = ts.to_id[&taxon_name];
        decomp.reordered_taxa[id]
    });
    let n = records.len(); // # of seqs
    let k = records[0].seq.len(); // # of columns
    let mut nchars_prefix = Array::<u32, _>::zeros((n + 1, k).f());
    for i in 1..n+1 {
        for j in 0..k {
            if i == 1 {
                nchars_prefix[[i, j]] = if records[i-1].seq[j] == b'-' { 0 } else { 1 };
            } else {
                nchars_prefix[[i, j]] =
                    nchars_prefix[[i - 1, j]] + if records[i-1].seq[j] == b'-' { 0 } else { 1 };
            }
        }
    }
    let subsets_root = outdir.join("subsets");
    let metadata_path = outdir.join("melt.json");
    create_dir_all(&subsets_root)?;
    for (i, &(lb, ub)) in decomp.decomposition_ranges.iter().enumerate() {
        let to_write = &records[lb..ub];
        let mut writer = BufWriter::new(File::create(subsets_root.join(format!("{}.afa", i)))?);
        for r in to_write {
            r.write_wrap(&mut writer, 60)?;
        }
    }
    let ctxt = CrucibleCtxt {
        nchars_partial_sum: nchars_prefix,
        hmm_ranges: decomp.decomposition_ranges,
    };
    let mut writer = BufWriter::new(File::create(metadata_path)?);
    serde_json::to_writer(&mut writer, &ctxt)?;
    // writer.write_all(&buf)?;
    Ok(())
}