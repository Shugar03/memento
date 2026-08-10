//! Spanish corpus fixtures (ES-first product language; REQ-MS-004 context).
//!
//! Shared by chunking tests (batch 4: bounds/overlap/determinism), FTS tests
//! (batch 3: accent handling) and ingest tests (batch 7). Texts are realistic
//! accented Spanish so tokenizer behavior is exercised, not elided.

/// A short corpus of accented Spanish paragraphs.
pub const SPANISH_CORPUS: [&str; 5] = [
    "La memoria es la facultad de recordar lo que ya no está, un río subterráneo que nunca deja de fluir.",
    "El conocimiento se construye con información, pero la sabiduría se teje con experiencia y reflexión.",
    "Cada recuerdo es una fotografía que la mente revela con luces y sombras, nunca idéntica a la anterior.",
    "Documentar una decisión es el primer paso hacia la transparencia: sin registro no hay rendición de cuentas.",
    "La atención plena nos permite distinguir lo urgente de lo importante, y dedicar memoria a ambos.",
];

/// The corpus as an owned `Vec<&'static str>` (the shape most tests want).
pub fn spanish_corpus() -> Vec<&'static str> {
    SPANISH_CORPUS.to_vec()
}

/// Accented/plain pairs that must match under the FTS ascii-folding filter
/// (e.g. searching "informacion" finds "información").
pub fn accent_pairs() -> Vec<(&'static str, &'static str)> {
    vec![
        ("memoria", "memória"),
        ("informacion", "información"),
        ("decision", "decisión"),
        ("explicacion", "explicación"),
        ("corazon", "corazón"),
    ]
}

/// A long Spanish document (~2.4k chars) for chunking bounds tests.
pub fn long_spanish_doc() -> &'static str {
    "En el principio fue el verbo, y el verbo fue la memoria. \
     Toda organización que aspira a aprender necesita conservar lo que hace, \
     lo que decide y lo que descarta. La memoria corporativa no es un archivo \
     muerto: es el tejido conectivo entre la experiencia pasada y la acción \
     futura. Por eso documentar no es un trámite; es la condición de \
     posibilidad de la mejora continua. Cuando un equipo registra sus \
     decisiones, construye un andamiaje sobre el cual las generaciones \
     siguientes pueden apoyarse sin repetir los mismos errores. \
     \
     La gestión del conocimiento institucional enfrenta tres desafíos \
     permanentes: la fragmentación de las fuentes, la obsolescencia de los \
     registros y la resistencia cultural a documentar. La fragmentación nace \
     de la multiplicidad de herramientas: un correo aquí, una reunión allá, \
     un documento de texto más allá. La obsolescencia surge cuando nadie \
     revisa ni actualiza lo archivado, y la resistencia cultural aparece \
     cuando documentar se percibe como tiempo perdido en lugar de inversión. \
     \
     Frente a estos desafíos, la tecnología ofrece respuestas parciales pero \
     valiosas: búsqueda semántica, resúmenes automáticos, registros \
     inmutables y recordatorios oportunos. Sin embargo, ninguna herramienta \
     sustituye la disciplina del hábito: documentar en el momento, con \
     precisión y con la intención de ser leído por alguien más. \
     \
     Esta es la tesis que sostiene el proyecto Memento: la memoria debe ser \
     un servicio, no un accidente. Un servicio que almacena, indexa y \
     recupera lo que la organización considera digno de recordar, respetando \
     al mismo tiempo el derecho a olvidar. Porque recordar sin criterio es \
     acumular; olvidar sin criterio es perder. La curaduría de la memoria \
     exige ambos gestos: conservar lo esencial y eliminar lo accesorio. \
     \
     En las páginas siguientes se desarrollan los principios de diseño, la \
     arquitectura del sistema y los protocolos operativos que hacen posible \
     esta visión. Cada sección intenta responder a una pregunta concreta: \
     qué guardar, cómo encontrarlo, quién tiene derecho a leerlo y cuándo \
     debe desaparecer. La respuesta final, como suele ocurrir, no es \
     tecnológica: es ética."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_is_accented_and_sized() {
        for (i, text) in SPANISH_CORPUS.iter().enumerate() {
            assert!(
                text.len() > 80,
                "corpus[{i}] should be a substantial paragraph"
            );
            assert!(
                text.chars().any(|c| c as u32 > 127),
                "corpus[{i}] should contain accented characters"
            );
        }
    }

    #[test]
    fn long_doc_exceeds_chunk_target() {
        // Chunking targets 256-270 tokens (REQ-MC-003): this fixture must
        // span several chunks so bounds tests are meaningful.
        assert!(long_spanish_doc().len() > 2000, "doc too short");
    }
}
