Symptome : une recherche litterale rend zero resultat alors que le terme figure dans une note.
Le jeton **azurite** identifie cette note.

Cause : l'index lexical n'avait pas ete reconstruit apres un import en masse.
Remede : reconstruire l'index, puis verifier le compte de correspondances.
