"""Pure CPU scoring module extracted cleanly from an HTTP handler."""


def score_candidates(records: list[dict[str, object]], query_terms: list[str]) -> list[dict[str, object]]:
    """Rank candidate items against query terms using token overlap and edit similarity."""
    terms = [t.strip().lower() for t in query_terms if t.strip()]
    results: list[dict[str, object]] = []
    for rec in records:
        name = str(rec.get("name", "")).lower()
        tags = str(rec.get("tags", "")).lower()
        base_weight = float(rec.get("weight", 1.0))
        hay = f"{name} {tags}"
        score = 0.0
        for term in terms:
            if term in hay:
                score += 2.0 * base_weight
            else:
                # Character bigram Jaccard similarity fallback for fuzzy matches
                a = {term[i : i + 2] for i in range(max(0, len(term) - 1))}
                b = {hay[i : i + 2] for i in range(max(0, len(hay) - 1))}
                if a and b:
                    score += (len(a & b) / len(a | b)) * base_weight
        results.append({"id": rec.get("id"), "score": round(score, 4)})
    results.sort(key=lambda item: float(item["score"]), reverse=True)
    return results[:10]
