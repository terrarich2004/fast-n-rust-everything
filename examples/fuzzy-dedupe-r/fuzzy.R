# Fuzzy string matching helpers.

# Edit distance: how many inserts, deletes or substitutions turn a into b.
levenshtein <- function(a, b) {
  a <- strsplit(a, "")[[1]]
  b <- strsplit(b, "")[[1]]
  if (length(a) < length(b)) {
    t <- a
    a <- b
    b <- t
  }
  previous <- 0:length(b)
  for (i in seq_along(a)) {
    current <- c(i, integer(length(b)))
    for (j in seq_along(b)) {
      current[j + 1] <- min(previous[j + 1] + 1, current[j] + 1, previous[j] + (a[i] != b[j]))
    }
    previous <- current
  }
  previous[length(previous)]
}

normalize <- function(name) {
  words <- strsplit(tolower(name), "[ \t\n\r]+")[[1]]
  paste(words[nzchar(words)], collapse = " ")
}

# Index pairs (counting from 1, as R does) of names within max_distance edits of each other.
find_duplicates <- function(names, max_distance = 2) {
  clean <- vapply(names, normalize, "", USE.NAMES = FALSE)
  pairs <- list()
  n <- length(clean)
  for (i in seq_len(max(n - 1, 0))) {
    for (j in (i + 1):n) {
      if (levenshtein(clean[i], clean[j]) <= max_distance) pairs[[length(pairs) + 1]] <- c(i, j)
    }
  }
  pairs
}
