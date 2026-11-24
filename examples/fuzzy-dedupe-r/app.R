# Find near-duplicate customer names, the slow way.
#     Rscript app.R
source("fuzzy.R")

FIRST <- c("Maria", "Giulia", "Sofia", "Luca", "Marco", "Matteo", "José", "Zoë", "Łukasz", "Aiko",
           "Chen", "Olga", "Ahmed", "Fatima", "Liam", "Emma", "Noah", "Ava", "Mateo", "Ingrid")
LAST <- c("Rossi", "Russo", "Ferrari", "Esposito", "Bianchi", "García", "Müller", "Nowak", "Tanaka", "Wang",
          "Ivanova", "Haddad", "Smith", "Johnson", "Brown", "Silva", "Kowalski", "Novák", "Dubois", "Larsen")

make_names <- function(count, seed = 7) {
  set.seed(seed)
  vapply(seq_len(count), function(k) {
    name <- paste(sample(FIRST, 1), sample(LAST, 1))
    if (runif(1) < 0.35) {
      i <- sample.int(nchar(name), 1)
      k <- sample.int(10, 1)
      substr(name, i, i) <- substr("aeiourstln", k, k)
    }
    name
  }, "")
}

if (sys.nframe() == 0) {
  names <- make_names(300)
  pairs <- find_duplicates(names)
  cat(length(names), "names,", length(pairs), "likely duplicate pairs\n")
  shown <- Filter(function(p) names[p[1]] != names[p[2]], pairs)
  for (p in head(shown, 5)) cat(" ", names[p[1]], "~", names[p[2]], "\n")
}
