# Basiliskode: arquitectura de referencia para sistemas de conocimiento

Este documento presenta la arquitectura de referencia de Basiliskode,
un proyecto de software diseñado para gestionar conocimiento personal y
organizacional. La arquitectura de referencia (reference architecture)
describe los componentes, sus responsabilidades y cómo se comunican.

## Principios de la arquitectura de referencia

La arquitectura de referencia de Basiliskode se basa en tres principios:
separación de responsabilidades, persistencia inmutable y recuperación
eficiente. Cada módulo tiene una responsabilidad única y se comunica con
el resto a través de interfaces explícitas.

La arquitectura de referencia no es una implementación: es un mapa. Los
equipos que implementan Basiliskode deben poder localizar cada
capacidad en el mapa y entender qué contrato la define.

## Capas del sistema

La arquitectura de referencia de Basiliskode distingue cinco capas:

1. La capa de dominio, con los conceptos centrales del negocio.
2. La capa de aplicación, con los casos de uso.
3. La capa de puertos, que define las interfaces hacia el exterior.
4. La capa de adaptadores, que implementa esas interfaces.
5. La capa de superficie, con los puntos de entrada del usuario.

Esta organización en capas permite sustituir un adaptador sin tocar el
núcleo, y probar la lógica de dominio sin depender de la
infraestructura.

## Recuperación y conocimiento

Una de las piezas centrales de la arquitectura de referencia de
Basiliskode es el subsistema de recuperación: ingesta, fragmentación,
indexación y búsqueda. El conocimiento se fragmenta en unidades
atómicas, se indexa con representaciones vectoriales y se recupera por
relevancia.

La arquitectura de referencia especifica el contrato de recuperación
pero no la implementación: el índice puede ser local o remoto, el
modelo de embeddings puede cambiar, siempre que se respete el contrato.

## Conclusión

La arquitectura de referencia de Basiliskode ofrece un vocabulario
común para equipos que construyen sistemas de conocimiento. Con sus
capas bien definidas y su contrato de recuperación explícito, sirve
como base para implementaciones escalables y mantenibles.
