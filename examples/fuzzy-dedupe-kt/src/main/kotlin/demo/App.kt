package demo

// Find near-duplicate customer names, the slow way.
//     gradle -q run

val FIRST = listOf("Maria", "Giulia", "Sofia", "Luca", "Marco", "Matteo", "José", "Zoë", "Łukasz", "Aiko",
    "Chen", "Olga", "Ahmed", "Fatima", "Liam", "Emma", "Noah", "Ava", "Mateo", "Ingrid")
val LAST = listOf("Rossi", "Russo", "Ferrari", "Esposito", "Bianchi", "García", "Müller", "Nowak", "Tanaka", "Wang",
    "Ivanova", "Haddad", "Smith", "Johnson", "Brown", "Silva", "Kowalski", "Novák", "Dubois", "Larsen")

fun makeNames(count: Int, seed: Long = 7): List<String> {
    val rng = java.util.Random(seed)
    return List(count) {
        var name = "${FIRST[rng.nextInt(FIRST.size)]} ${LAST[rng.nextInt(LAST.size)]}"
        if (rng.nextDouble() < 0.35) {
            val i = rng.nextInt(name.length)
            name = name.substring(0, i) + "aeiourstln"[rng.nextInt(10)] + name.substring(i + 1)
        }
        name
    }
}

fun main() {
    val names = makeNames(5000)
    val pairs = findDuplicates(names)
    println("${names.size} names, ${pairs.size} likely duplicate pairs")
    pairs.filter { (i, j) -> names[i] != names[j] }.take(5).forEach { (i, j) -> println("  ${names[i]} ~ ${names[j]}") }
}
