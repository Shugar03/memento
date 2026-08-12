# Clawfy ImageGen: diagnóstico de generación de imágenes

Este documento describe el sistema Clawfy ImageGen y el diagnóstico de
los problemas más comunes en la generación de imágenes.

## Qué es Clawfy ImageGen

Clawfy ImageGen es un servicio de generación de imágenes por descripción
de texto. Permite crear ilustraciones, mockups y variaciones visuales a
partir de un prompt. El diagnóstico del sistema cubre tanto la calidad
de las imágenes generadas como el rendimiento del servicio.

## Diagnóstico de calidad

El diagnóstico de Clawfy ImageGen evalúa la calidad de las imágenes en
cuatro dimensiones:

1. Fidelidad al prompt: ¿la imagen refleja lo pedido?
2. Coherencia visual: ¿los elementos mantienen una estética unificada?
3. Detalle: ¿se pierde información en áreas complejas?
4. Seguridad: ¿se respetan las restricciones de contenido?

Cada dimensión se puntúa con una rúbrica y se registra para alimentar
el modelo de mejora continua.

## Diagnóstico de rendimiento

Además de la calidad, el diagnóstico de Clawfy ImageGen monitorea la
latencia, la tasa de éxito y el uso de recursos de cada generación. Un
cuadro de diagnóstico muestra en tiempo real el estado del servicio y
alerta cuando las métricas se degradan.

## Flujo de diagnóstico

El flujo de diagnóstico de Clawfy ImageGen es:

1. El usuario envía una solicitud de generación.
2. El servicio genera la imagen y captura las métricas.
3. El diagnóstico evalúa la imagen y el rendimiento.
4. Se emite un informe de diagnóstico con recomendaciones.

## Conclusión

Clawfy ImageGen combina generación de imágenes con un diagnóstico
riguroso de calidad y rendimiento. Ese diagnóstico es lo que permite
mejorar los prompts, los modelos y la infraestructura de forma
sistemática.
