# Arquitectura de agentes conversacionales

Este documento describe cómo diseñar la arquitectura de agentes
conversacionales que atienden clientes en múltiples canales. Una
arquitectura sólida separa el entendimiento del lenguaje, la gestión
del diálogo y la integración con los sistemas de negocio.

## Componentes de un agente conversacional

Un agente conversacional moderno se compone de varias capas:

1. El canal de entrada, que normaliza los mensajes del usuario.
2. El entendimiento del lenguaje, que extrae intención y entidades.
3. El gestor de diálogo, que decide la siguiente acción.
4. Las integraciones, que ejecutan acciones en sistemas externos.

Cada capa debe poder probarse y evolucionar por separado. Un agente
conversacional es un sistema distribuido disfrazado de chat.

## El enfoque omnicanal

La arquitectura omnicanal garantiza que el mismo agente conversacional
atienda al cliente en web, WhatsApp, redes sociales y teléfono con un
estado compartido. El cliente no debería tener que repetir su problema
al cambiar de canal.

Para lograrlo, la sesión conversacional se modela como un estado
persistente y agnóstico del canal. Los adaptadores de canal traducen
ese estado al formato específico de cada plataforma.

## Agente conversacional y conocimiento

Un agente conversacional que responde desde un corpus de conocimiento
necesita recuperación de información: buscar los fragmentos relevantes
antes de generar la respuesta. La calidad de la respuesta depende tanto
del modelo como de la calidad de la recuperación.

El patrón habitual es: embebido del mensaje, búsqueda de los
fragmentos más cercanos y composición de la respuesta a partir de esos
fragmentos. Sin una buena recuperación, el agente conversacional
alucina o repite.

## Observabilidad y mejora continua

Los agentes conversacionales requieren registro de conversaciones,
métricas de resolución y ciclos de revisión. Cada interacción fallida
es una oportunidad de mejorar la base de conocimiento o las reglas del
diálogo.

## Conclusión

La arquitectura de agentes conversacionales combina entendimiento del
lenguaje, gestión de estado omnicanal y recuperación de conocimiento.
Diseñada en capas, escala de un canal a muchos sin reescribir el
núcleo.
