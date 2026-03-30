"""AI workload: distributed TF-IDF feature extraction.

Each line of the corpus is treated as a document.  Workers compute per-shard
term frequency (TF) and document frequency (DF) counts; the reducer merges
them, computes IDF = log(N / df), and outputs the top-50 TF-IDF scored terms.

Functions called by the DAG JSON via runner.py:
  tfidf_map(in_slot, out_slot)   — per-shard TF + DF computation
  tfidf_reduce(in_slot)          — global merge + TF-IDF scoring + output
"""

import shm
import math

_SEP = '\x1f'
_MIN_WORD_LEN = 3   # skip short stop-word candidates


def tfidf_map(in_slot, out_slot):
    """Compute term frequency and document frequency for the records in
    stream slot `in_slot`.  Emits three record types to stream slot `out_slot`:

      docs=<N>                  — number of documents (lines) in this shard
      tf<SEP><word><SEP><count> — total occurrences of word across shard
      df<SEP><word><SEP><count> — number of docs in shard containing word
    """
    tf = {}
    df = {}
    doc_count = 0

    for _origin, rec in shm.read_all_stream_records(in_slot):
        seen_in_doc = set()
        for token in rec.decode('utf-8', errors='replace').lower().split():
            word = ''.join(c for c in token if c.isalpha())
            if len(word) >= _MIN_WORD_LEN:
                tf[word] = tf.get(word, 0) + 1
                seen_in_doc.add(word)
        for word in seen_in_doc:
            df[word] = df.get(word, 0) + 1
        doc_count += 1

    shm.append_stream_data(out_slot, ('docs=' + str(doc_count)).encode())
    for word, count in tf.items():
        shm.append_stream_data(out_slot,
                               ('tf' + _SEP + word + _SEP + str(count)).encode())
    for word, count in df.items():
        shm.append_stream_data(out_slot,
                               ('df' + _SEP + word + _SEP + str(count)).encode())


def tfidf_reduce(in_slot):
    """Merge TF/DF records from all shards in stream slot `in_slot`, compute
    TF-IDF scores, and write the top-50 terms to I/O slot 1.

    TF-IDF formula:  score(w) = tf(w) * log(N / df(w))
    """
    tf_total = {}
    df_total = {}
    total_docs = 0

    for _origin, rec in shm.read_all_stream_records(in_slot):
        s = rec.decode('utf-8', errors='replace')
        if s.startswith('docs='):
            total_docs += int(s[5:])
        else:
            parts = s.split(_SEP)
            if len(parts) == 3:
                kind, word, val = parts[0], parts[1], int(parts[2])
                if kind == 'tf':
                    tf_total[word] = tf_total.get(word, 0) + val
                elif kind == 'df':
                    df_total[word] = df_total.get(word, 0) + val

    if total_docs == 0:
        return

    scores = {
        word: tf_total[word] * math.log(total_docs / max(df_total.get(word, 1), 1))
        for word in tf_total
    }
    top50 = sorted(scores.items(), key=lambda x: -x[1])[:50]

    shm.write_output(b'=== tfidf_results ===')
    shm.write_output(('total_docs=' + str(total_docs)).encode())
    shm.write_output(('unique_terms=' + str(len(scores))).encode())
    shm.write_output(b'--- top_50_tfidf_terms ---')
    for word, score in top50:
        shm.write_output((word + ': ' + ('%.4f' % score)).encode())
