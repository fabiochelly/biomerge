"""French text bank for the synthetic corpus (written for this benchmark, no external source)."""
import random

PARAS = [
    "Le comité de direction s'est réuni le mardi matin pour examiner les résultats du trimestre écoulé. Les ventes ont progressé dans toutes les régions, à l'exception du secteur nord où la concurrence est restée très vive.",
    "Chaque collaborateur recevra avant la fin du mois une note détaillée précisant les nouvelles modalités de remboursement des frais professionnels. Les justificatifs devront être conservés pendant une durée minimale de trois ans.",
    "La rénovation de la façade commencera au printemps si les conditions météorologiques le permettent. Les riverains seront prévenus par courrier une semaine avant l'installation des échafaudages.",
    "Le contrat prévoit une période d'essai de deux mois, renouvelable une fois avec l'accord écrit des deux parties. Pendant cette période, chacune peut mettre fin à la relation sans indemnité.",
    "Les élèves de la classe de seconde ont visité le musée des sciences et découvert une exposition consacrée à l'histoire de l'astronomie. Le guide a répondu patiemment à toutes leurs questions.",
    "En cas de litige, le client s'adressera d'abord au service après-vente, qui dispose d'un délai de quinze jours pour proposer une solution amiable. À défaut d'accord, le médiateur de la consommation pourra être saisi.",
    "La bibliothèque municipale prolonge ses horaires d'ouverture pendant les vacances scolaires. Un atelier de lecture à voix haute est proposé chaque mercredi après-midi aux enfants de six à dix ans.",
    "Après plusieurs semaines de négociations, les partenaires sociaux ont trouvé un compromis sur l'organisation du temps de travail. L'accord sera soumis au vote des salariés au début du mois prochain.",
    "Le jardin botanique abrite plus de quatre mille espèces végétales venues des cinq continents. Une serre tropicale, récemment restaurée, accueille désormais une collection remarquable d'orchidées.",
    "Le rapport souligne la nécessité d'investir dans la formation continue afin d'accompagner la transformation numérique des métiers. Il recommande également de simplifier les procédures internes.",
    "Les travaux de voirie entraîneront des perturbations sur la ligne de bus numéro douze entre la gare et l'hôpital. Des itinéraires de déviation seront indiqués par une signalisation temporaire.",
    "Le musicien a interprété une sonate méconnue devant un public conquis, puis il a terminé la soirée par une improvisation inspirée des chansons populaires de son enfance.",
]

WORDS = ("le la les un une des du de et à au aux pour avec sans dans sur sous entre vers chez "
         "maison jardin rapport contrat client société projet équipe réunion budget dossier "
         "rivière montagne village semaine matin soir année période service bureau document "
         "important nouvelle récent prochain général financier technique régional possible "
         "présente examine propose prévoit indique confirme rappelle souligne accepte organise "
         "rapidement simplement également ensuite toujours souvent désormais peut-être "
         "électricité célèbre déjà où être forêt château élève hôpital français").split()


def sentence(rng, n=None):
    n = n or rng.randint(8, 18)
    w = [rng.choice(WORDS) for _ in range(n)]
    s = " ".join(w)
    return s[0].upper() + s[1:] + "."


def paragraph(rng, sentences=None):
    return " ".join(sentence(rng) for _ in range(sentences or rng.randint(2, 5)))


def rng(seed):
    return random.Random(seed)
